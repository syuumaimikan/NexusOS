# Working in NexusOS

- Remove obsolete generated files as work progresses. Use `scripts/clean.ps1`
  for owned disposable test payloads; preserve evidence, persistent disk images,
  user settings, source files and dependency caches. Resolve and check every
  recursive deletion target, and never traverse junctions or symbolic links.
- Keep `scripts/run.ps1` and affected build/test scripts usable. Verify relevant
  successful boots and failure handling before reporting completion. `build.ps1
  -Clean` preserves persistent storage; program updates refresh only FAT32.
- After completing and verifying a coherent change, commit and push it to the
  configured remote automatically, as requested by the user. Stage only files
  and hunks belonging to the work; preserve unrelated edits. Never force-push.
- Distinguish CPU Vulkan/OpenGL and QEMU display tests from physical GPU
  acceleration. Steam sign-in and RTX 5060 acceleration are not yet implemented;
  do not report them as working until demonstrated on the requested platform.
