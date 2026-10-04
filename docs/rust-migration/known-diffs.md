# Known differences between the C# and Rust builds

Every place the Rust build deliberately behaves differently from C# `2026.10.03.2`, and why.
The parity harness must show no diff that is not listed here.

| Area | C# behaviour | Rust behaviour | Why |
|---|---|---|---|
| Config binding | An empty (`null` from a cleared dashboard field) or unconvertible value for a number, bool or enum setting throws when the section is bound, breaking the whole section until `settings.json` is fixed. | The value is skipped with a warning and the next lower source (env, `appsettings.json`, then the class default) supplies it. | The C# behaviour is a latent bug the dashboard can trigger; see `config.md` §1. |
| Config binding | Enum values combine with commas as a flags OR (`"Track,Album"`). | Not supported; such a value is skipped as unconvertible. | No Octo setting is a flags enum. |
| Packages | Swashbuckle serves `/swagger` in Development. | No Swagger UI. | Never served in any shipped configuration (`packages.md`). |
| Audio tools | `Process.Kill(entireProcessTree: true)` on a timeout or failure. | The ffmpeg/fpcalc process alone is killed (also on drop). | Neither tool starts child processes, so the tree is the process. |
| Spectrum check | Sample rate and duration come from TagLib#. | They come from lofty. | No TagLib in Rust; a file one library reads and the other refuses changes only "the file's format could not be read" (no opinion). |
| Spectrum check | An I/O error reading ffmpeg's pipes escapes `AnalyzeAsync` as an exception, against its own "never throws" contract. | No opinion, "the check failed", with a warning. | `analyze` cannot throw; the documented contract is kept. |
| Fingerprinting | A `"fingerprint"` in fpcalc's JSON that is neither a string nor null throws `InvalidOperationException` out of `FingerprintAsync`. | Treated as output that is not fpcalc's shape: Unavailable (no opinion). | No real fpcalc writes it; an exception into the download loop is what the class exists to prevent. |
| Fingerprinting | A negative `timeoutSeconds` throws `ArgumentOutOfRangeException` after fpcalc has started. | Times out at once: Unavailable. | A setting value cannot be allowed to panic the download loop. |
