# Known differences between the C# and Rust builds

Every place the Rust build deliberately behaves differently from C# `2026.10.03.2`, and why.
The parity harness must show no diff that is not listed here.

| Area | C# behaviour | Rust behaviour | Why |
|---|---|---|---|
| Config binding | An empty (`null` from a cleared dashboard field) or unconvertible value for a number, bool or enum setting throws when the section is bound, breaking the whole section until `settings.json` is fixed. | The value is skipped with a warning and the next lower source (env, `appsettings.json`, then the class default) supplies it. | The C# behaviour is a latent bug the dashboard can trigger; see `config.md` §1. |
| Config binding | Enum values combine with commas as a flags OR (`"Track,Album"`). | Not supported; such a value is skipped as unconvertible. | No Octo setting is a flags enum. |
| Packages | Swashbuckle serves `/swagger` in Development. | No Swagger UI. | Never served in any shipped configuration (`packages.md`). |
