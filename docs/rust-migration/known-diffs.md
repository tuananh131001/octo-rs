# Known differences between the C# and Rust builds

Every place the Rust build deliberately behaves differently from C# `2026.10.03.2`, and why.
The parity harness must show no diff that is not listed here.

| Area | C# behaviour | Rust behaviour | Why |
|---|---|---|---|
| Config binding | An empty (`null` from a cleared dashboard field) or unconvertible value for a number, bool or enum setting throws when the section is bound, breaking the whole section until `settings.json` is fixed. | The value is skipped with a warning and the next lower source (env, `appsettings.json`, then the class default) supplies it. | The C# behaviour is a latent bug the dashboard can trigger; see `config.md` §1. |
| Config binding | Enum values combine with commas as a flags OR (`"Track,Album"`). | Not supported; such a value is skipped as unconvertible. | No Octo setting is a flags enum. |
| Packages | Swashbuckle serves `/swagger` in Development. | No Swagger UI. | Never served in any shipped configuration (`packages.md`). |
| Unicode data | `char.IsLetter`, casing, NFKC and the regex classes use .NET 9's tables (Unicode 15.1) and the host's ICU. | `octo_core::common::dotnet` reproduces .NET's rules (UTF-16 units, simple casing with the İ/ı/ſ exceptions, .NET's `\b` word set), but over Rust's own Unicode tables (16/17). | Only characters assigned after Unicode 15.1 can classify or case differently. Everything else was compared against .NET 9 code point by code point. |
| Regex case-insensitivity | Patterns built with `IgnoreCase` but not `CultureInvariant` follow the process culture: under en-US, `(?i)i` also matches `İ` (U+0130), so for example `[İ]` reads as a part number. | Always the invariant culture's equivalences (`i`↔`I`, `k`↔`K`↔`K`, `s`↔`S` only). | The container sets no `LANG`, so production C# ran with the invariant culture; this keeps that behaviour wherever Octo runs. |
| `downloads-history.json` | A JSON `null` in a non-nullable C# `string`/list property (`Artist`, `Title`, `Path`, `Format`, `Source`, `DownloadedAt`, and the `TagReport` strings and lists) is read as null and written back as `null`. | Read as the empty value and written back as `""` / `[]` / `{}`. | Only a hand-edited file has such a null; reading it at all (rather than failing the whole log) is what matters. |
