# Isolation and resource limits

The service is a normal isolated userspace process with no filesystem, network,
spawn, process-control or display capability. Core/IPC are allocation-free.
Limits: 256-byte transport buffers, 64 service messages, 64 runtime requests,
two observation attempts per task, 500 ms cooperative tool deadline, 60 s idle
wait. System observations are non-blocking syscalls; a future blocking tool must
run in a supervised worker because the core cannot preempt adapter code.

These bounds are not a general sandbox for untrusted native code. CPU time,
address-space size, process count, directory scope, network destinations and
wall-clock termination must be OS/broker enforced before terminal.execute.
The service can still call ambient allocation/clock/log syscalls like any process;
capability isolation is not a claim of full syscall filtering. Initial probe
checks process disconnect/teardown, not arbitrary-code sandbox escape resistance.
