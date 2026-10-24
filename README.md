# rpt-cs

Direct rendering of Crystal Reports `.rpt` files to **PDF** from .NET — no SAP runtime, no Windows dependency, no Crystal DLLs.

> Sulla falsa riga di [rpt-rs](https://github.com/MrSrsen/rpt-rs) (l'engine, Rust): **rpt-cs** è la porta verso il mondo .NET. Il motore è rpt-rs; questo repo è il shim ABI + il wrapper C#.

## Com'è fatto

| Pezzo | Ruolo |
|-------|-------|
| `native/` | Crate Rust `rpt-cs` (cdylib): ABI C minimale e stateless su rpt-rs — bytes in, PDF out |
| `dotnet/RptCs/` | Libreria .NET (`RptCs`): P/Invoke + resolver nativo + facade `CrystalReport` |
| `dotnet/RptCs.Tests/` | Test end-to-end (stesse fixture lato Rust e lato .NET) |
| `samples/` | Fixture neutrali (da rpt-rs; nessun corpus proprietario) |

Zero dipendenze managed su .NET 10 (`System.Text.Json` è in-box); su netstandard2.0 solo il pacchetto System.Text.Json. Il binario nativo (~10 MB/RID) include font Liberation/DejaVu: output riproducibile byte-per-byte.

## Uso

```csharp
using RptCs;

using var report = CrystalReport.Load("invoice.rpt");   // il decode fallisce qui, se il file è rotto
report.SetParameterValue("DocKey", 12345);
report.SetDataSource(mainTable);                        // righe del report principale
report.SetSubreportData("SubPagamenti", subTable);      // righe per subreport (chiave = Info.Subreports)

PdfExport export = report.Export(new PdfExportOptions
{
    Locale = "it-IT",
    Conformance = PdfConformance.PdfA2b,                // claim verificato: se non rispettabile, fallisce elencando i gap
});
File.WriteAllBytes("invoice.pdf", export.Pdf);
foreach (var w in export.Diagnostics) { /* fidelity warnings: righe scartate, type coercion, font sostituiti… */ }
```

## Contratto ABI (statoless)

- `cre_version()`, `cre_inspect(bytes, len)`, `cre_render(bytes, len, request_json, len)`, `cre_result_free(result)`
- Request JSON: `params`, `locale`, `fonts` (`bundled`/`system`), `conformance` (`none`, `pdfa1b…3a`, `pdfua1`), `title`, `language`, `alt`, `asOf`, `dataset`, `subreports`
- Result: codice (0 ok, 1 invalid, 2 decode, 3 pdf, 4 internal) + buffer PDF + JSON `{pages, diagnostics}` / `{"error"}`

Il wrapper .NET isola tutto: evolve rpt-rs? Si aggiorna solo lo shim.

## Sorgenti dati (cosa aspettarsi)

1. **Saved data** — righe salvate nel file: render zero-config.
2. **`SetDataSource(DataTable)`** — righe live del report principale (il caso reale: report senza saved data).
3. **`SetSubreportData(name, DataTable)`** — righe per ogni subreport, per nome. Chiavi ignote → warning, mai silenzio.

## Build & test

```sh
cd native      && cargo build --release && cargo test --release
cd dotnet      && dotnet test           # il resolver trova il binario appena costruito
```

Il test runner .NET usa `RPT_CS_NATIVE_PATH` (impostato da un module initializer verso `native/target/{release,debug}`); per un consumo manuale impostarla a mano o copiare il binario in `runtimes/<rid>/native/`.

## Packaging

`dotnet pack dotnet/RptCs` produce il NuGet con `runtimes/<rid>/native/rpt_cs.(dll|so|dylib)`; il resolver .NET 5+ sonda `runtimes/<rid>/native` e l'app dir. RID v1: win-x64 (linux-x64/osx da CI).

## Licenze

MPL-2.0 (derivato da rpt-rs). Notizie di terze parti (krilla, cosmic-text, Liberation/DejaVu OFL…) in `THIRD-PARTY-NOTICES.md`. Crystal Reports è un marchio di SAP SE; rpt-cs è un'implementazione indipendente e non affiliata.