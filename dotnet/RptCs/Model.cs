using System.Collections.Generic;

namespace RptCs;

/// <summary>How the PDF claims conformance. Levels A and PDF/UA add a structure tree and are
/// checked claims: an unmet requirement (missing alt text, language, title) fails the export,
/// naming every gap — the engine never writes a document that lies about itself.</summary>
public enum PdfConformance
{
    /// <summary>An ordinary PDF, no claim (default).</summary>
    None = 0,
    /// <summary>PDF/A-1b (ISO 19005-1 level B) — PDF 1.4, forbids transparency and 16-bit images.</summary>
    PdfA1b,
    /// <summary>PDF/A-2b (ISO 19005-2 level B) — PDF 1.7, permits transparency. Usual archival default.</summary>
    PdfA2b,
    /// <summary>PDF/A-3b (ISO 19005-3 level B) — PDF/A-2b plus attachment rights (not emitted).</summary>
    PdfA3b,
    /// <summary>PDF/A-1a — level B plus a structure tree.</summary>
    PdfA1a,
    /// <summary>PDF/A-2a — level B plus a structure tree.</summary>
    PdfA2a,
    /// <summary>PDF/A-3a — level B plus a structure tree.</summary>
    PdfA3a,
    /// <summary>PDF/UA-1 (ISO 14289-1), the accessibility standard.</summary>
    PdfUa1,
}

/// <summary>Which face library backs the render. <see cref="Bundled"/> ships the engine's own
/// Liberation/DejaVu set (byte-reproducible output on every machine); <see cref="System"/> resolves
/// and embeds the host's installed faces (fidelity when the report's real families are installed).</summary>
public enum FontSource
{
    Bundled = 0,
    System = 1,
}

/// <summary>Export options. All optional; defaults produce a plain, reproducible PDF.</summary>
public sealed class PdfExportOptions
{
    /// <summary>Conformance claim (default: none).</summary>
    public PdfConformance Conformance { get; set; } = PdfConformance.None;
    /// <summary>Render locale as a BCP-47-ish tag (<c>it-IT</c>, <c>de-DE</c>…); drives number/date
    /// separators and names. Unknown tags fall back to en-US with a warning.</summary>
    public string? Locale { get; set; }
    /// <summary>Font library (default: bundled).</summary>
    public FontSource Fonts { get; set; } = FontSource.Bundled;
    /// <summary>Document title override (needed by PDF/UA-1 when the report has none).</summary>
    public string? Title { get; set; }
    /// <summary>Document natural language (needed by level-A / PDF/UA conformance).</summary>
    public string? Language { get; set; }
    /// <summary>Alternate text per picture/chart object name (needed by level-A / PDF/UA).</summary>
    public IDictionary<string, string>? AltText { get; set; }
    /// <summary>As-of instant resolving <c>CurrentDate</c>/<c>Today</c>/… — freeze for reproducible renders.</summary>
    public System.DateTime? AsOf { get; set; }
}

/// <summary>One report parameter as declared by the file.</summary>
public sealed class ParameterInfo
{
    public string Name = "";
    /// <summary>Declared value kind: string, number, currency, boolean, date, time, datetime.</summary>
    public string Type = "";
    public bool AllowMultiple;
    public bool Optional;
}

/// <summary>One data table as declared by the file.</summary>
public sealed class TableInfo
{
    public string Name = "";
    public string Alias = "";
}

/// <summary>Report metadata returned by <see cref="CrystalReport.Info"/>.</summary>
public sealed class ReportInfo
{
    public string Title = "";
    public string Author = "";
    /// <summary>Whether the file carries usable saved data (renders zero-config).</summary>
    public bool SavedData;
    public IReadOnlyList<ParameterInfo> Parameters = new ParameterInfo[0];
    /// <summary>Subreport names (dotted paths for nested ones) — the keys for <see cref="CrystalReport.SetSubreportData"/>.</summary>
    public IReadOnlyList<string> Subreports = new string[0];
    public IReadOnlyList<TableInfo> Tables = new TableInfo[0];
}

/// <summary>One fidelity warning collected during render. The pipeline fails open: a dropped row,
/// a substituted type, an unsupported object — each is reported here instead of silently blanking.</summary>
public sealed class RenderDiagnostic
{
    /// <summary><c>warning</c> or <c>error</c>.</summary>
    public string Severity = "";
    /// <summary>Engine kind (e.g. <c>RecordSelection</c>, <c>TypeCoercion</c>, <c>FontSubstituted</c>).</summary>
    public string Kind = "";
    public string Message = "";
    public string? Source;
}

/// <summary>The result of one export: PDF bytes plus the render's diagnostics.</summary>
public sealed class PdfExport
{
    public byte[] Pdf = new byte[0];
    public int Pages;
    public IReadOnlyList<RenderDiagnostic> Diagnostics = new RenderDiagnostic[0];
}