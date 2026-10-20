# CTF traces

`wk-heartbeat-u` is an LTTng-UST 2.0 trace of eight per-cpu streams, copied
unchanged from babeltrace 2's test suite
(`tests/common-data/ctf-traces/1/succeed/wk-heartbeat-u`, commit
`5f3a7824b14fa675794d10fdea60fbd4d8939afb` of
<https://github.com/efficios/babeltrace>, MIT licensed). Its expected reading is
babeltrace's own, `tests/plugins/sink.text.details/default-ctf1-mip0.expect`
in the same commit: 20 events of `heartbeat:msg`, the first at
1 967 640 734 196 cycles of a 1 GHz `monotonic` clock, from `vtid` 3214 of
`vpid` 3208, on cpu 2.
