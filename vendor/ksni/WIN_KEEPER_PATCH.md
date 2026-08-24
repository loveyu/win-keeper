## WinKeeper patch

This is `ksni` 0.3.6 from crates.io with a local service-loop shutdown fix.

The upstream loop selected only `Some(...)` values from its D-Bus signal stream and handle
channel. Dropping the last handle therefore left the service alive, and closing the session D-Bus
later caused `futures_util::select!` to panic because every selected future had completed. The
WinKeeper release profile uses `panic = "abort"`, so that background panic terminated the process.

The patched loop handles `None` explicitly, closes the D-Bus connection, and exits. The blocking
integration test `dropping_handle_shuts_down_service` covers the original failure.
