# Agent and task lifecycle

Implemented: one Runtime per service channel, fixed session 1, monotonically
increasing nonzero request IDs. Each request selects a typed tool. Execution
moves through Ready, Executing, Verifying, Complete or a rejection/failure.
Cancellation is terminal in the library; service cancellation currently means
closing the unique peer or supervised process kill, not a wire cancel command.
Each runtime permits 64 requests. Rejected valid requests consume IDs/budget.

This is a tool-task state machine, not a natural-language agent. A future agent
must own a goal, context provenance, bounded task graph and typed results. DAG
persistence must carry schema/version and validate dependencies/cycles on reload;
never reload grants from a task file. Multi-agent delegation must attenuate
capabilities and preserve task/session correlation. No such scheduler or durable
DAG is currently implemented.
