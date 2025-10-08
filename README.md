# srecon25-usl
Demo service developed to demonstrate USL for a SRECon talk.


## Test setup

To obtain the results that were used during the talk, the following setup was used.

        ┌──────────────────────┐          ┌──────────────────────┐
        │   EC2 Instance 1     │          │   EC2 Instance 2     │
        │                      │          │                      │
        │   ╔═════════════╗    │          │   ╔═════════════╗    │
        │   ║   Service   ║    │◄─────────┤   ║ Test Harness║    │
        │   ║             ║    │  HTTP    │   ║             ║    │
        │   ║  taskset    ║    │  Load    │   ║    wrk      ║    │
        │   ║ (CPU 0-N)   ║    │          │   ║ (load gen)  ║    │
        │   ╚═════════════╝    │          │   ╚═════════════╝    │
        │                      │          │                      │
        │  Scale CPU cores     │          │  Generate load       │
        │  with taskset        │          │  with wrk tool       │
        └──────────────────────┘          └──────────────────────┘
`wrk` was used to generate load from the test machine to the service.
For example,
```
wrk -t128 -c6000 -d30s http://A.B.C.D:8080/work
wrk -t128 -c6000 -d30s http://A.B.C.D:8080/contention
wrk -t128 -c6000 -d30s http://A.B.C.D:8080/coherence
```
An automation script was used that would iteratively vary the concurrency factor `-c` and the number of threads `-t` across load stages.

`taskset` was used(in Linux) to control th enumber of CPU cores available to the service. For example, 
`taskset -c 0-3 ./target/release/usl_demo_service` would run the program on 4 pinned cores only. 

The instrumentation in the service emits useful metrics via the `/summary` endpoint that looks like this.
```
curl http://172.31.22.71:8080/summary

{
  "num_cpus": 3,
  "total_requests_served": 26583,
  "service_time_min_ms": 16,
  "service_time_max_ms": 358,
  "service_time_avg_ms": 78.91942218711206,
  "in_flight_current": 1,
  "in_flight_max": 119,
  "in_flight_avg": 41.696610615807096,
  "db_counter_value": 26639
}
```
