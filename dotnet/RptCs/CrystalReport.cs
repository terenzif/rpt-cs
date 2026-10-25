using System;
using System.Collections.Generic;
using System.Data;
using System.Globalization;
using System.IO;
using System.Text.Json;

namespace RptCs;

/// <summary>
/// A loaded Crystal Reports <c>.rpt</c> document: parameters, data, and PDF export — the open-source
/// shape of <c>CrystalDecisions.CrystalReports.Engine.ReportDocument</c>, backed by the rpt-rs engine.
/// One instance is one in-memory report; every <see cref="Export"/> call re-renders from the current
/// parameters and data.
/// </summary>
public sealed class CrystalReport
{
    private readonly byte[] _bytes;
    private readonly Dictionary<string, object?> _parameters = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, DataTable> _subreportData = new(StringComparer.OrdinalIgnoreCase);
    private DataTable? _mainData;

    /// <summary>Metadata read from the file: title, author, parameters, subreport names, saved-data status.</summary>
    public ReportInfo Info { get; }

    /// <summary>The diagnostics of the last export (fidelity warnings the engine surfaced).</summary>
    public IReadOnlyList<RenderDiagnostic> Warnings { get; private set; } = Array.Empty<RenderDiagnostic>();

    private CrystalReport(byte[] bytes, ReportInfo info)
    {
        _bytes = bytes;
        Info = info;
    }

    /// <summary>Load a report from a file path.</summary>
    public static CrystalReport Load(string path)
    {
        return Load(File.ReadAllBytes(path));
    }

    /// <summary>Load a report from a stream (read to end; the stream stays open).</summary>
    public static CrystalReport Load(Stream stream)
    {
        using var buffer = new MemoryStream();
        stream.CopyTo(buffer);
        return Load(buffer.ToArray());
    }

    /// <summary>Load a report from decoded bytes. The engine decodes immediately — malformed files
    /// fail here with a decode error, not at export time.</summary>
    public static CrystalReport Load(byte[] bytes)
    {
        var result = Native.Inspect(bytes);
        if (result.Code != 0)
        {
            throw new RptCsException(result.Code, ErrorMessage(result.Json, "decoding failed"));
        }
        return new CrystalReport(bytes, ParseInfo(result.Json));
    }

    /// <summary>Set a report parameter value. Pass a scalar, or an enumerable for multi-value parameters.</summary>
    public void SetParameterValue(string name, object? value)
    {
        _parameters[name] = value;
    }

    /// <summary>Feed live rows to the main report (SDK: <c>SetDataSource</c>). Overrides the file's
    /// saved data; without it, a report without saved data renders only its static bands.</summary>
    public void SetDataSource(DataTable table)
    {
        _mainData = table;
    }

    /// <summary>Feed live rows to one subreport, keyed by its name from <see cref="ReportInfo.Subreports"/>
    /// (SDK: <c>Subreports[i].SetDataSource</c>). Unknown names are reported, not swallowed.</summary>
    public void SetSubreportData(string subreportName, DataTable table)
    {
        _subreportData[subreportName] = table;
    }

    /// <summary>Render and return the PDF bytes, with the default options.</summary>
    public byte[] ExportToPdf()
    {
        return Export().Pdf;
    }

    /// <summary>Render and return the PDF bytes.</summary>
    public byte[] ExportToPdf(PdfExportOptions options)
    {
        return Export(options).Pdf;
    }

    /// <summary>Render and return the full export: PDF bytes, page count, diagnostics.</summary>
    public PdfExport Export(PdfExportOptions? options = null)
    {
        var request = BuildRequest(options);
        var result = Native.Render(_bytes, request);
        if (result.Code != 0)
        {
            throw new RptCsException(result.Code, ErrorMessage(result.Json, "export failed"));
        }
        using var json = JsonDocument.Parse(result.Json);
        var export = new PdfExport
        {
            Pdf = result.Pdf,
            Pages = json.RootElement.GetProperty("pages").GetInt32(),
            Diagnostics = ParseDiagnostics(json.RootElement),
        };
        Warnings = export.Diagnostics;
        return export;
    }

    /// <summary>Render and write the PDF to a stream.</summary>
    public void ExportToPdf(Stream target, PdfExportOptions? options = null)
    {
        var pdf = ExportToPdf(options ?? new PdfExportOptions());
        target.Write(pdf, 0, pdf.Length);
    }

    // ------------------------------------------------------------------
    // Request building
    // ------------------------------------------------------------------

    private byte[] BuildRequest(PdfExportOptions? options)
    {
        var opt = options ?? new PdfExportOptions();
        var request = new Dictionary<string, object?>();
        if (_parameters.Count > 0)
        {
            var parameters = new Dictionary<string, object?>();
            foreach (var pair in _parameters)
            {
                parameters[pair.Key] = ToJsonValue(pair.Value);
            }
            request["params"] = parameters;
        }
        request["fonts"] = opt.Fonts == FontSource.System ? "system" : "bundled";
        request["conformance"] = ConformanceName(opt.Conformance);
        if (!string.IsNullOrEmpty(opt.Locale))
        {
            request["locale"] = opt.Locale;
        }
        if (!string.IsNullOrEmpty(opt.Title))
        {
            request["title"] = opt.Title;
        }
        if (!string.IsNullOrEmpty(opt.Language))
        {
            request["language"] = opt.Language;
        }
        if (opt.AltText is { Count: > 0 })
        {
            request["alt"] = opt.AltText;
        }
        if (opt.AsOf.HasValue)
        {
            request["asOf"] = new DateTimeOffset(DateTime.SpecifyKind(opt.AsOf.Value, DateTimeKind.Unspecified), TimeSpan.Zero).ToUnixTimeSeconds();
        }
        if (_mainData != null)
        {
            request["dataset"] = DatasetToJson(_mainData);
        }
        if (_subreportData.Count > 0)
        {
            var subreports = new Dictionary<string, object?>();
            foreach (var pair in _subreportData)
            {
                subreports[pair.Key] = DatasetToJson(pair.Value);
            }
            request["subreports"] = subreports;
        }
        return JsonSerializer.SerializeToUtf8Bytes(request);
    }

    private static string ConformanceName(PdfConformance conformance)
    {
        return conformance switch
        {
            PdfConformance.PdfA1b => "pdfa1b",
            PdfConformance.PdfA2b => "pdfa2b",
            PdfConformance.PdfA3b => "pdfa3b",
            PdfConformance.PdfA1a => "pdfa1a",
            PdfConformance.PdfA2a => "pdfa2a",
            PdfConformance.PdfA3a => "pdfa3a",
            PdfConformance.PdfUa1 => "pdfua1",
            _ => "none",
        };
    }

    private static object? ToJsonValue(object? value)
    {
        if (value == null || value is DBNull)
        {
            return null;
        }
        if (value is string s)
        {
            return s;
        }
        if (value is bool b)
        {
            return b;
        }
        if (value is byte[] bytes)
        {
            return Convert.ToBase64String(bytes);
        }
        if (value is DateTime dt)
        {
            return dt.ToString("yyyy-MM-dd'T'HH:mm:ss", CultureInfo.InvariantCulture);
        }
#if NET6_0_OR_GREATER
        if (value is DateOnly d)
        {
            return d.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);
        }
        if (value is TimeOnly t)
        {
            return t.ToString("HH:mm:ss", CultureInfo.InvariantCulture);
        }
#endif
        if (value is char c)
        {
            return c.ToString();
        }
        if (value is Guid g)
        {
            return g.ToString();
        }
        if (value is System.Collections.IEnumerable enumerable)
        {
            var list = new List<object?>();
            foreach (var item in enumerable)
            {
                list.Add(ToJsonValue(item));
            }
            return list;
        }
        if (value is IFormattable)
        {
            return value; // numbers: System.Text.Json writes the runtime type as a JSON number
        }
        return value.ToString();
    }

    private static object DatasetToJson(DataTable table)
    {
        var columns = new List<object?>();
        foreach (DataColumn column in table.Columns)
        {
            columns.Add(new Dictionary<string, object?>
            {
                ["name"] = column.ColumnName,
                ["type"] = ColumnType(column.DataType),
            });
        }
        var rows = new List<object?>();
        foreach (DataRow row in table.Rows)
        {
            var cells = new List<object?>();
            foreach (var cell in row.ItemArray)
            {
                cells.Add(ToJsonValue(cell));
            }
            rows.Add(cells);
        }
        return new Dictionary<string, object?>
        {
            ["columns"] = columns,
            ["rows"] = rows,
        };
    }

    private static string ColumnType(Type type)
    {
        if (type == typeof(string))
        {
            return "string";
        }
        if (type == typeof(byte[]))
        {
            return "blob";
        }
        if (type == typeof(bool))
        {
            return "boolean";
        }
        if (type == typeof(DateTime))
        {
            return "datetime";
        }
#if NET6_0_OR_GREATER
        if (type == typeof(DateOnly))
        {
            return "date";
        }
        if (type == typeof(TimeOnly))
        {
            return "time";
        }
#endif
        if (type == typeof(decimal) || type == typeof(double) || type == typeof(float) ||
            type == typeof(sbyte) || type == typeof(short) || type == typeof(int) || type == typeof(long) ||
            type == typeof(byte) || type == typeof(ushort) || type == typeof(uint) || type == typeof(ulong))
        {
            return "number";
        }
        return "string";
    }

    // ------------------------------------------------------------------
    // Response parsing
    // ------------------------------------------------------------------

    private static string ErrorMessage(byte[] json, string fallback)
    {
        try
        {
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.TryGetProperty("error", out var error))
            {
                return error.GetString() ?? fallback;
            }
        }
        catch (JsonException)
        {
            // fall through to the fallback message
        }
        return fallback;
    }

    private static ReportInfo ParseInfo(byte[] json)
    {
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        var parameters = new List<ParameterInfo>();
        foreach (var p in root.GetProperty("parameters").EnumerateArray())
        {
            parameters.Add(new ParameterInfo
            {
                Name = p.GetProperty("name").GetString() ?? "",
                Type = p.GetProperty("type").GetString() ?? "string",
                AllowMultiple = p.GetProperty("allowMultiple").GetBoolean(),
                Optional = p.GetProperty("optional").GetBoolean(),
            });
        }
        var subreports = new List<string>();
        foreach (var s in root.GetProperty("subreports").EnumerateArray())
        {
            subreports.Add(s.GetString() ?? "");
        }
        var tables = new List<TableInfo>();
        foreach (var t in root.GetProperty("tables").EnumerateArray())
        {
            tables.Add(new TableInfo
            {
                Name = t.GetProperty("name").GetString() ?? "",
                Alias = t.GetProperty("alias").GetString() ?? "",
            });
        }
        return new ReportInfo
        {
            Title = root.GetProperty("title").GetString() ?? "",
            Author = root.GetProperty("author").GetString() ?? "",
            SavedData = root.GetProperty("savedData").GetBoolean(),
            Parameters = parameters,
            Subreports = subreports,
            Tables = tables,
        };
    }

    private static List<RenderDiagnostic> ParseDiagnostics(JsonElement root)
    {
        var diagnostics = new List<RenderDiagnostic>();
        if (!root.TryGetProperty("diagnostics", out var array) || array.ValueKind != JsonValueKind.Array)
        {
            return diagnostics;
        }
        foreach (var d in array.EnumerateArray())
        {
            diagnostics.Add(new RenderDiagnostic
            {
                Severity = d.TryGetProperty("severity", out var severity) ? severity.GetString() ?? "" : "",
                Kind = d.TryGetProperty("kind", out var kind) ? kind.GetString() ?? "" : "",
                Message = d.TryGetProperty("message", out var message) ? message.GetString() ?? "" : "",
                Source = d.TryGetProperty("source", out var source) && source.ValueKind == JsonValueKind.String
                    ? source.GetString()
                    : null,
            });
        }
        return diagnostics;
    }
}