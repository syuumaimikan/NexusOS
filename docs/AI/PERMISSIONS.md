# Permission evaluation

Permission identifies Read, Write, Execute, Network, ProcessControl,
SystemConfiguration or Privileged. Level classifies Safe, ConfirmRequired,
Privileged or Blocked. Classification and authorization are separate:
fs.read is Safe in classification but Unsupported in this service because no
resource capability has been provisioned. Safe never implies ambient authority.

Only system.info is allowlisted. There is no approval bit, grant setter,
permission file, or privileged escape hatch in the request protocol. The service
has only a parent channel and creates a waitset. Unexpected transferred handles
are closed; if CLOSE is missing the process exits to release them at teardown.

An eventual confirmation workflow must be implemented by a trusted broker/UI,
not by adding a boolean to the model request. Until that exists, ConfirmRequired
and Privileged never transition to execution. See AI_SECURITY.md for binding,
revocation and delegation requirements.
