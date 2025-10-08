use axum::{extract::State, http::StatusCode, response::Json, routing::get, Router};
use lazy_static::lazy_static;
use num_cpus;
use rand::Rng;
use rusqlite::{Connection, Result};
use serde::Serialize;
use std::hint::black_box;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration};
use tracing::error;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// --- Global Database Connection ---
//Mutex ensures only one request holds it at a time
lazy_static! {
    static ref DB_CONNECTION: Mutex<Connection> = {
        let conn = Connection::open_in_memory().expect("Failed to open database");
        Mutex::new(conn)
    };
}

// --- Configuration Constants ---
const WORK_LOAD: u64 = 50_000_000;

const CONTENTION_IO_SLEEP_MS: u64 = 3; // 5ms of simulated I/O work
const CONTENTION_IO_JITTER_PERCENT: u64 = 20; // Add +/- 20% jitter to the sleep time
const CONTENTION_PARALLEL_LOAD: u64 = (2_000_000) / 2;
const CONTENTION_DB_CPU_LOAD: u64 = 150_000; // HEAVY CPU work *inside* the lock.

const COHERENCE_BASE_LOAD: u64 = 20_000_000;
// This factor is multiplied by N*(N-1) to simulate crosstalk.
const COHERENCE_FACTOR: u64 = 100_000;

// --- Data Structures ---

struct AppStats {
    total_requests_served: AtomicUsize,
    service_time_total_ms: AtomicU64,
    service_time_min_ms: AtomicU64,
    service_time_max_ms: AtomicU64,
    in_flight_max: AtomicUsize,
    in_flight_sum: AtomicUsize,
}

/// Represents the shared state for our application.
#[derive(Clone)]
struct AppState {
    /// A mutex to simulate a contended resource. Used by the /contention endpoint.
    // contention_lock: Arc<Mutex<()>>,
    //  A semaphore to limit concurrency for the "serial" part of the contention workload.
    /// This acts as a pool of resources, one for each available CPU core.
    contention_limiter: Arc<Semaphore>,
    /// An atomic counter for in-flight requests. Used by all endpoints.
    in_flight_requests: Arc<AtomicUsize>,
    /// A container for all our performance stats.
    stats: Arc<AppStats>,
}

#[derive(Serialize)]
struct WorkResponse {
    message: String,
    duration_ms: u128,
    result: u64,
}

#[derive(Serialize)]
struct SummaryResponse {
    num_cpus: usize,
    total_requests_served: usize,
    service_time_min_ms: u64,
    service_time_max_ms: u64,
    service_time_avg_ms: f64,
    in_flight_current: usize,
    in_flight_max: usize,
    in_flight_avg: f64,
    db_counter_value: i64,
}

/// RAII guard to decrement the in-flight request counter.
struct RequestGuard(Arc<AtomicUsize>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

// --- Main Application ---

/// Sets up the database table if it doesn't exist.
fn setup_database() -> Result<()> {
    let conn = DB_CONNECTION.blocking_lock();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS counter (
             id INTEGER PRIMARY KEY,
             value INTEGER NOT NULL
         )",
        [],
    )?;
    conn.execute(
        "INSERT INTO counter (id, value) VALUES (1, 1)
             ON CONFLICT(id) DO UPDATE SET value=1",
        [],
    )?;
    Ok(())
}

async fn perform_db_work() -> Result<usize> {
    // Acquire the async lock on the global database connection.
    // Requests will queue here when the lock is held.
    let mut conn_guard = DB_CONNECTION.lock().await;
    perform_cpu_work(CONTENTION_DB_CPU_LOAD);
    let tx = conn_guard.transaction()?;
    let updated_rows = tx.execute("UPDATE counter SET value = value + 1 WHERE id = 1", [])?;
    tx.commit()?;
    Ok(updated_rows)
}

/// A pure CPU-bound function using a tight loop of dependent integer and
/// bitwise operations. This is very difficult for the compiler to optimize away.
#[inline(never)]
fn perform_cpu_work(iterations: u64) -> u64 {
    let mut acc: u64 = 0;
    // This loop uses a data dependency on `acc` to prevent autovectorization.
    // The operations are simple integer/bitwise ops that are fast on the CPU.
    for i in 0..iterations {
        acc = acc.wrapping_add(((i as u64) ^ acc).rotate_left((i % 31) as u32));
    }
    // Use black_box to ensure the result is considered "used" by the compiler.
    black_box(acc)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "usl_demo_service=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    let available_cpus = num_cpus::get();
    info!("Initializing with {} available CPUs.", available_cpus);
    info!("Setting up the database...");
    tokio::task::spawn_blocking(|| setup_database())
        .await
        .expect("Failed to join blocking task")
        .expect("Failed to initialize database");
    info!("Database setup complete.");

    // Create the shared state, initializing stats appropriately.
    let shared_state = AppState {
        // contention_lock: Arc::new(Mutex::new(())),
        // Arbitrary choice of 12 semaphores to mimick real-world downstream pressure like DBs.
        contention_limiter: Arc::new(Semaphore::new(12)),
        in_flight_requests: Arc::new(AtomicUsize::new(0)),
        stats: Arc::new(AppStats {
            total_requests_served: AtomicUsize::new(0),
            service_time_total_ms: AtomicU64::new(0),
            service_time_min_ms: AtomicU64::new(u64::MAX),
            service_time_max_ms: AtomicU64::new(0),
            in_flight_max: AtomicUsize::new(0),
            in_flight_sum: AtomicUsize::new(0),
        }),
    };

    let app = Router::new()
        .route("/work", get(work_handler))
        .route("/contention", get(contention_handler))
        .route("/coherence", get(coherence_handler))
        .route("/summary", get(summary_handler))
        .with_state(shared_state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    info!("Listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

// --- Route Handlers ---

/// A generic function to update stats after a request is complete.
fn update_stats(state: &AppState, start_time: Instant, n_inflight: usize) {
    let duration_ms = start_time.elapsed().as_millis() as u64;
    state
        .stats
        .total_requests_served
        .fetch_add(1, Ordering::SeqCst);
    state
        .stats
        .service_time_total_ms
        .fetch_add(duration_ms, Ordering::SeqCst);
    state
        .stats
        .service_time_min_ms
        .fetch_min(duration_ms, Ordering::SeqCst);
    state
        .stats
        .service_time_max_ms
        .fetch_max(duration_ms, Ordering::SeqCst);
    state
        .stats
        .in_flight_max
        .fetch_max(n_inflight, Ordering::SeqCst);
    state
        .stats
        .in_flight_sum
        .fetch_add(n_inflight, Ordering::SeqCst);
}

/// Handler for the new `/summary` endpoint.
async fn summary_handler(State(state): State<AppState>) -> (StatusCode, Json<SummaryResponse>) {
    let total_reqs = state.stats.total_requests_served.load(Ordering::SeqCst);
    let service_time_total = state.stats.service_time_total_ms.load(Ordering::SeqCst);
    let mut min_service_time = state.stats.service_time_min_ms.load(Ordering::SeqCst);
    let in_flight_sum = state.stats.in_flight_sum.load(Ordering::SeqCst);
    let db_value_result = tokio::task::spawn_blocking(|| {
        let conn = DB_CONNECTION.blocking_lock();
        conn.query_row("SELECT value FROM counter WHERE id = 1", [], |row| {
            row.get(0)
        })
    })
    .await;

    let db_counter_value: i64 = match db_value_result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            error!("Failed to query DB for summary: {}", e);
            -1 // Use -1 to indicate an error in fetching the value.
        }
        Err(e) => {
            error!("Failed to spawn blocking DB task: {}", e);
            -1 // Use -1 to indicate an error.
        }
    };
    if min_service_time == u64::MAX {
        min_service_time = 0;
    }

    let available_cpus = num_cpus::get();
    info!("Initializing with {} available CPUs.", available_cpus);

    let summary = SummaryResponse {
        num_cpus: available_cpus,
        total_requests_served: total_reqs,
        service_time_min_ms: min_service_time,
        service_time_max_ms: state.stats.service_time_max_ms.load(Ordering::SeqCst),
        service_time_avg_ms: if total_reqs > 0 {
            service_time_total as f64 / total_reqs as f64
        } else {
            0.0
        },
        in_flight_current: state.in_flight_requests.load(Ordering::SeqCst),
        in_flight_max: state.stats.in_flight_max.load(Ordering::SeqCst),
        in_flight_avg: if total_reqs > 0 {
            in_flight_sum as f64 / total_reqs as f64
        } else {
            0.0
        },
        db_counter_value,
    };

    (StatusCode::OK, Json(summary))
}

async fn work_handler(State(state): State<AppState>) -> (StatusCode, Json<WorkResponse>) {
    let n = state.in_flight_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = RequestGuard(state.in_flight_requests.clone());
    let start = Instant::now();

    let result = perform_cpu_work(WORK_LOAD);

    update_stats(&state, start, n);
    let duration_ms = start.elapsed().as_millis();
    let response = WorkResponse {
        message: format!("Completed pure CPU work with load {}", WORK_LOAD),
        duration_ms,
        result,
    };
    (StatusCode::OK, Json(response))
}

async fn contention_handler(State(state): State<AppState>) -> (StatusCode, Json<WorkResponse>) {
    let n = state.in_flight_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = RequestGuard(state.in_flight_requests.clone());
    let start = Instant::now();
    let (sleep1, sleep2) = {
        let mut rng = rand::thread_rng();
        let jitter = (CONTENTION_IO_SLEEP_MS * CONTENTION_IO_JITTER_PERCENT) / 100;
        let min_sleep = CONTENTION_IO_SLEEP_MS.saturating_sub(jitter);
        let max_sleep = CONTENTION_IO_SLEEP_MS.saturating_add(jitter);
        (
            rng.gen_range(min_sleep..=max_sleep),
            rng.gen_range(min_sleep..=max_sleep),
        )
    };
    perform_cpu_work(CONTENTION_PARALLEL_LOAD);
    // Phase 1: Simulate initial I/O work with random jitter
    sleep(Duration::from_millis(sleep1)).await;

    // Phase 2: The contended, serial part of the work
    let _ = perform_db_work().await;

    // Phase 3: Simulate final I/O work with random jitter
    sleep(Duration::from_millis(sleep2)).await;

    // perform_cpu_work(CONTENTION_PARALLEL_LOAD);
    // Serial part of the work, now handled by the database lock
    // let _db_result = perform_db_work().await;
    // {
    //     // let _lock_guard = state.contention_lock.lock().await;
    //     // perform_cpu_work(CONTENTION_SERIAL_LOAD);
    //     // Acquire a permit from the semaphore. If all permits are taken (i.e., more
    //     // concurrent requests than CPUs), this line will wait asynchronously
    //     // until a permit is released.
    //     let _permit = state.contention_limiter.acquire().await.unwrap();

    //     // This work only runs once a permit is acquired.
    //     perform_cpu_work(CONTENTION_SERIAL_LOAD);
    // } // The `_permit` guard is dropped here, releasing the permit back to the semaphore.

    // //}
    let result = perform_cpu_work(CONTENTION_PARALLEL_LOAD);

    update_stats(&state, start, n);
    let duration_ms = start.elapsed().as_millis();
    let response = WorkResponse {
        message: format!(
            "Completed work with parallel load {} and serial load {}",
            CONTENTION_PARALLEL_LOAD * 2,
            CONTENTION_DB_CPU_LOAD
        ),
        duration_ms,
        result,
    };
    (StatusCode::OK, Json(response))
}

async fn coherence_handler(State(state): State<AppState>) -> (StatusCode, Json<WorkResponse>) {
    let n = state.in_flight_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = RequestGuard(state.in_flight_requests.clone());
    let start = Instant::now();

    let coherency_penalty = if n > 1 {
        (n as u64 * (n - 1) as u64).saturating_mul(COHERENCE_FACTOR)
    } else {
        0
    };
    let total_load = COHERENCE_BASE_LOAD.saturating_add(coherency_penalty);
    let result = perform_cpu_work(total_load);

    update_stats(&state, start, n);
    let duration_ms = start.elapsed().as_millis();
    let response = WorkResponse {
        message: format!(
            "Completed coherence-bound work for N={} with total load {}",
            n, total_load
        ),
        duration_ms,
        result,
    };
    (StatusCode::OK, Json(response))
}
