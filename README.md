# rpt-cs

Direct rendering of Crystal Reports `.rpt` files to **PDF** from .NET — no SAP runtime, no Windows dependency, no Crystal DLLs.

> Sulla falsa riga di [rpt-rs](https://github.com/MrSrsen/rpt-rs) (l'engine, Rust): **rpt-cs** è la porta verso il mondo .NET. Il motore è rpt-rs; questo repo è il shim ABI + il wrapper C#.

## Com'è fatto

| Pezzo | Ruolo |
|-------|-------|
| `native/` | Crate Rust `rpt-cs` (cdylib): ABI C minimale e stateless su rpt-rs — bytes in, PDF out |
| `dotnet/RptCs/` | Libreria .NET (`RptCs`): P/Invoke + resolver nativo + facade `CrystalReport` |
| `dotnet/RptCs.Tests/` | Test end-to-end (stesse fixture lato Rust e lato .NET) |
| `dotnet/RptCs.Compat/` | Contratto di compilazione per consumatori .NET Framework 4.8 / C# 7.3: la build del progetto è la verifica che la superficie pubblica resti consumabile |
| `samples/` | Fixture neutrali (da rpt-rs; nessun corpus proprietario) |

Zero dipendenze managed su .NET 10 (`System.Text.Json` è in-box); su netstandard2.0 solo il pacchetto System.Text.Json. Il binario nativo (~8.5 MB win-x64, ~7.6 MB win-x86 — profilo release tarato min-size: `opt-level="z"`, fat LTO, `strip`) include font Liberation/DejaVu: output riproducibile byte-per-byte.

## Build & test

Un solo comando: i target MSBuild (`dotnet/RptCs.Native.props`) invocano `cargo` per conto loro — build nativa per ogni RID dell'OS host (Windows: win-x64 **e** win-x86), test Rust agganciati a `dotnet test`, nativo incluso nel NuGet in `runtimes/<rid>/native`.

```sh
dotnet test dotnet/RptCs.slnx -c Release   # cargo test + test .NET (prima esecuzione: compila anche il nativo)
dotnet build dotnet/RptCs.slnx             # build nativa + managed
dotnet pack  dotnet/RptCs -c Release       # NuGet con runtimes/<rid>/native
```

- **Visual Studio**: apri `dotnet/RptCs.slnx` — build e test eseguono anche il crate Rust (serve `cargo`/`rustup` nel PATH).
- Su Windows serve il targeting pack MSVC x86 per win-x86; i target rust extra vengono aggiunti in automatico (`rustup target add`).
- Il profilo `release` è tarato **min-size** (`opt-level="z"`, fat LTO, `strip="symbols"`); il rendering resta deterministico byte-per-byte.
- Escape hatch: `/p:RptCsSkipNativeBuild=true` salta l'orchestrazione nativa (resolver/`RPT_CS_NATIVE_PATH` come prima).

Il test runner .NET usa `RPT_CS_NATIVE_PATH` (impostato da un module initializer verso `native/target/<triple>/{release,debug}`); per un consumo manuale impostarla a mano o copiare il binario in `runtimes/<rid>/native/`.

## Packaging

`dotnet pack dotnet/RptCs -c Release` produce il NuGet con `runtimes/<rid>/native/rpt_cs.(dll|so|dylib)`: un pacchetto per OS host — Windows porta win-x64 **e** win-x86; linux-x64 e osx (x64 + arm64) dalla CI. Il resolver .NET 5+ sonda `runtimes/<rid>/native` e l'app dir.

## Uso

```csharp
using RptCs;

using var report = CrystalReport.Load("invoice.rpt");   // il decode fallisce qui, se il file è rotto
report.SetParameterValue("DocKey", 12345);
report.SetDataSource(dataSet);                          // push model: DataSet multi-tabella (join sui links del report)
report.SetDataSource(mainTable);                        // oppure una DataTable sola
report.SetSubreportData("SubPagamenti", subTable);      // righe per subreport (chiave = Info.Subreports)

foreach (var field in report.Info.Fields)               // i riferimenti {tabella.campo} che il report usa
    Console.WriteLine(field.QualifiedName);

PdfExport export = report.Export(new PdfExportOptions
{
    Locale = "it-IT",
    Conformance = PdfConformance.PdfA2b,                // claim verificato: se non rispettabile, fallisce elencando i gap
});
File.WriteAllBytes("invoice.pdf", export.Pdf);
// oppure: using var pdfStream = report.ExportToStream();  // MemoryStream pronto a zero
foreach (var w in export.Diagnostics) { /* fidelity warnings: righe scartate, type coercion, font sostituiti… */ }
```

## Contratto ABI (statoless)

- `cre_version()`, `cre_inspect(bytes, len)`, `cre_render(bytes, len, request_json, len)`, `cre_result_free(result)`
- Request JSON: `params`, `locale`, `fonts` (`bundled`/`system`), `conformance` (`none`, `pdfa1b…3a`, `pdfua1`), `title`, `language`, `alt`, `asOf`, `dataset`, `subreports`
- `dataset`/`subreports[i]` accettano due forme: flat (`columns` + `rows`, una tabella) o push model (`{"tables": {nome: Dataset}}`) — nel secondo caso le tabelle del report si legano per nome/alias e il motore le joina lungo i links salvati nel file; tabelle senza dati risolvono a Null e finiscono in diagnostics, mai silenzio
- Inspect JSON: `title`, `author`, `savedData`, `parameters`, `fields` (i riferimenti dei field object, deduplicati), `subreports`, `tables`
- Result: codice (0 ok, 1 invalid, 2 decode, 3 pdf, 4 internal) + buffer PDF + JSON `{pages, diagnostics}` / `{"error"}`

Il wrapper .NET isola tutto: evolve rpt-rs? Si aggiorna solo lo shim.

## Sorgenti dati (cosa aspettarsi)

1. **Saved data** — righe salvate nel file: render zero-config.
2. **`SetDataSource(DataTable)`** — righe live del report principale (il caso reale: report senza saved data).
3. **`SetDataSource(DataSet)`** — push model multi-tabella: ogni tabella del report si lega alla DataTable con lo stesso nome/alias (case-insensitive) e il motore le joina lungo i links del file (inner/left/right/full, operatori `=`/<>/…; operatori non confrontabili numerico/testo vengono segnalati). Tabelle senza righe o mai raggiunte dal grafo → colonne Null + warning; colonne in più ignorate e conteggiate; conversioni tolleranti tra date/datetime/time.
4. **`SetSubreportData(name, DataTable)`** — righe per ogni subreport, per nome. Chiavi ignote → warning, mai silenzio.

## Licenze

MPL-2.0 (derivato da rpt-rs). Notizie di terze parti (krilla, cosmic-text, Liberation/DejaVu OFL…) in `THIRD-PARTY-NOTICES.md`. Crystal Reports è un marchio di SAP SE; rpt-cs è un'implementazione indipendente e non affiliata.