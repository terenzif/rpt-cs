# rpt-cs

Direct rendering of Crystal Reports `.rpt` files to **PDF** from .NET — no SAP runtime, no Windows dependency, no Crystal DLLs.

> In the spirit of [rpt-rs](https://github.com/MrSrsen/rpt-rs) (the engine, in Rust): **rpt-cs** is the bridge into .NET. The engine is rpt-rs; this repo is the ABI shim + the C# wrapper.

## Architecture

| Component | Role |
|-----------|------|
| `native/` | Rust crate `rpt-cs` (cdylib): minimal stateless C ABI over rpt-rs — bytes in, PDF out |
| `dotnet/RptCs/` | .NET library (`RptCs`): P/Invoke + native resolver + `CrystalReport` facade |
| `dotnet/RptCs.Tests/` | End-to-end tests (same fixtures on the Rust and .NET sides) |
| `dotnet/RptCs.Compat/` | Compile contract for .NET Framework 4.8 / C# 7.3 consumers: building the project verifies the public surface stays consumable |
| `samples/` | Neutral fixtures (from rpt-rs; no proprietary corpus) |

Zero managed dependencies on .NET 10 (`System.Text.Json` is in-box); on netstandard2.0 only the System.Text.Json package. The native binary (~8.5 MB win-x64, ~7.6 MB win-x86 — release profile tuned for min-size: `opt-level="z"`, fat LTO, `strip`) embeds the Liberation/DejaVu fonts: byte-for-byte reproducible output.

## Build & test

One command: the MSBuild targets (`dotnet/RptCs.Native.props`) invoke `cargo` themselves — native build for every RID of the host OS (Windows: win-x64 **and** win-x86), Rust tests hooked into `dotnet test`, native binary included in the NuGet under `runtimes/<rid>/native`.

```sh
dotnet test dotnet/RptCs.slnx -c Release   # cargo test + .NET tests (first run: also builds the native binary)
dotnet build dotnet/RptCs.slnx             # native + managed build
dotnet pack  dotnet/RptCs -c Release       # NuGet with runtimes/<rid>/native
```

- **Visual Studio**: open `dotnet/RptCs.slnx` — build and test also run the Rust crate (`cargo`/`rustup` must be on the PATH).
- On Windows, the MSVC x86 targeting pack is required for win-x86; extra Rust targets are added automatically (`rustup target add`).
- The `release` profile is tuned for **min-size** (`opt-level="z"`, fat LTO, `strip="symbols"`); rendering stays byte-for-byte deterministic.
- Escape hatch: `/p:RptCsSkipNativeBuild=true` skips the native orchestration (resolver/`RPT_CS_NATIVE_PATH` as before).

The .NET test runner uses `RPT_CS_NATIVE_PATH` (set by a module initializer to `native/target/<triple>/{release,debug}`); for manual consumption, set it yourself or copy the binary into `runtimes/<rid>/native/`.

## Packaging

`dotnet pack dotnet/RptCs -c Release` produces the NuGet with this layout:

```
RptCs.<version>.nupkg
├── lib/netstandard2.0/RptCs.dll            (+ System.Text.Json for net48 consumers)
├── lib/net10.0/RptCs.dll
├── build/ + buildTransitive/RptCs.targets  (.NET Framework only: copies the native binary to the output)
└── runtimes/<rid>/native/rpt_cs.(dll|so|dylib)
```

One package per host OS — Windows ships **win-x64 and win-x86**; linux-x64 and osx (x64 + arm64) packages are produced when packing on those platforms. The .NET 5+ resolver probes `runtimes/<rid>/native` and the app dir; .NET Framework 4.x has no `runtimes/` probing, so the MSBuild target in the package copies `rpt_cs.dll` next to the assembly: RID chosen from `$(Platform)` (x86 → win-x86, otherwise win-x64; override with `RptCsNativeRid`). Legacy packages.config projects (they don't process `build/`): copy the dll manually.

## Usage

```csharp
using RptCs;

using var report = CrystalReport.Load("invoice.rpt");   // decoding fails here if the file is broken
report.SetParameterValue("DocKey", 12345);
report.SetDataSource(dataSet);                          // push model: multi-table DataSet (joins along the report links)
report.SetDataSource(mainTable);                        // or a single DataTable
report.SetSubreportData("SubPagamenti", subTable);      // rows per subreport (key = Info.Subreports)

foreach (var field in report.Info.Fields)               // the {table.field} references the report uses
    Console.WriteLine(field.QualifiedName);

PdfExport export = report.Export(new PdfExportOptions
{
    Locale = "it-IT",
    Conformance = PdfConformance.PdfA2b,                // verified claim: if it can't be honored, the export fails listing the gaps
});
File.WriteAllBytes("invoice.pdf", export.Pdf);
// or: using var pdfStream = report.ExportToStream();   // ready-to-use MemoryStream
foreach (var w in export.Diagnostics) { /* fidelity warnings: dropped lines, type coercion, substituted fonts… */ }
```

## ABI contract (stateless)

- `cre_version()`, `cre_inspect(bytes, len)`, `cre_render(bytes, len, request_json, len)`, `cre_result_free(result)`
- Request JSON: `params`, `locale`, `fonts` (`bundled`/`system`), `conformance` (`none`, `pdfa1b…3a`, `pdfua1`), `title`, `language`, `alt`, `asOf`, `dataset`, `subreports`
- `dataset`/`subreports[i]` accept two forms: flat (`columns` + `rows`, one table) or push model (`{"tables": {name: Dataset}}`) — in the latter case the report tables bind by name/alias and the engine joins them along the links saved in the file; tables without data resolve to Null and end up in diagnostics, never silently
- Inspect JSON: `title`, `author`, `savedData`, `parameters`, `fields` (field object references, deduplicated), `subreports`, `tables`
- Result: code (0 ok, 1 invalid, 2 decode, 3 pdf, 4 internal) + PDF buffer + JSON `{pages, diagnostics}` / `{"error"}`

The .NET wrapper isolates all of it: when rpt-rs evolves, only the shim gets updated.

## Data sources (what to expect)

1. **Saved data** — rows saved in the file: zero-config rendering.
2. **`SetDataSource(DataTable)`** — live rows for the main report (the real-world case: reports without saved data).
3. **`SetDataSource(DataSet)`** — multi-table push model: each report table binds to the DataTable with the same name/alias (case-insensitive) and the engine joins them along the links in the file (inner/left/right/full, operators `=`/<>/…; numeric/text operators that cannot be compared are flagged). Tables without rows or never reached by the graph → Null columns + warning; extra columns are ignored and counted; tolerant conversions between date/datetime/time.
4. **`SetSubreportData(name, DataTable)`** — rows for each subreport, by name. Unknown keys → warning, never silent.

## Licenses

MPL-2.0 (derived from rpt-rs). Third-party notices (krilla, cosmic-text, Liberation/DejaVu OFL…) in `THIRD-PARTY-NOTICES.md`. Crystal Reports is a trademark of SAP SE; rpt-cs is an independent, unaffiliated implementation.