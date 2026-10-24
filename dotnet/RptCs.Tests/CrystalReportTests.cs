using System;
using System.Data;
using System.IO;
using System.Linq;
using RptCs;
using Xunit;

namespace RptCs.Tests;

public class CrystalReportTests
{
    private static string SamplePath(string name)
    {
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir != null; dir = dir.Parent)
        {
            var candidate = Path.Combine(dir.FullName, "samples", name);
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }
        throw new FileNotFoundException($"sample {name} not found");
    }

    private static byte[] SampleBytes(string name) => File.ReadAllBytes(SamplePath(name));

    private static DataTable TeamsTable()
    {
        var table = new DataTable("teams");
        table.Columns.Add("teams.name", typeof(string));
        table.Columns.Add("teams.wins", typeof(int));
        table.Columns.Add("teams.pct", typeof(double));
        table.Columns.Add("teams.active", typeof(bool));
        table.Columns.Add("teams.founded", typeof(DateTime));
        table.Rows.Add("Alpha", 12, 0.625, true, new DateTime(1998, 4, 1));
        table.Rows.Add("Beta", 7, 0.438, false, DBNull.Value);
        return table;
    }

    [Fact]
    public void Load_inspects_metadata()
    {
        var report = CrystalReport.Load(SampleBytes("sports_teams.rpt"));
        Assert.True(report.Info.SavedData, "fixture carries saved data");
        Assert.NotNull(report.Info.Title);
        Assert.NotEmpty(report.Info.Tables);
    }

    [Fact]
    public void Export_saved_data_produces_a_pdf()
    {
        var report = CrystalReport.Load(SampleBytes("sports_teams.rpt"));
        var export = report.Export();
        Assert.True(export.Pdf.AsSpan().StartsWith("%PDF"u8), "output must start with the PDF header");
        Assert.True(export.Pages >= 1);
    }

    [Fact]
    public void Export_is_reproducible_with_frozen_as_of()
    {
        var options = new PdfExportOptions { AsOf = new DateTime(2026, 1, 1, 12, 0, 0) };
        var report = CrystalReport.Load(SampleBytes("sports_teams.rpt"));
        var first = report.Export(options).Pdf;
        var second = report.Export(options).Pdf;
        Assert.True(first.AsSpan().SequenceEqual(second.AsSpan()), "same inputs must produce identical bytes");
    }

    [Fact]
    public void Export_with_live_datatable()
    {
        var report = CrystalReport.Load(SampleBytes("sports_teams.rpt"));
        report.SetDataSource(TeamsTable());
        var export = report.Export();
        Assert.True(export.Pdf.AsSpan().StartsWith("%PDF"u8));
        Assert.True(export.Pages >= 1);
    }

    [Fact]
    public void Export_with_subreport_data()
    {
        var report = CrystalReport.Load(SampleBytes("top5_usa_with_sub.rpt"));
        Assert.NotEmpty(report.Info.Subreports);
        var sub = new DataTable("sub");
        sub.Columns.Add("sub.dummy", typeof(string));
        sub.Rows.Add("row one");
        sub.Rows.Add("row two");
        report.SetSubreportData(report.Info.Subreports[0], sub);
        var export = report.Export();
        Assert.True(export.Pdf.AsSpan().StartsWith("%PDF"u8));
        Assert.True(export.Pages >= 1);
    }

    [Fact]
    public void Unknown_subreport_is_reported_not_swallowed()
    {
        var report = CrystalReport.Load(SampleBytes("top5_usa_with_sub.rpt"));
        var sub = new DataTable("sub");
        sub.Columns.Add("x", typeof(string));
        report.SetSubreportData("NoSuchSubreport", sub);
        report.Export();
        Assert.Contains(report.Warnings, w => w.Message.Contains("NoSuchSubreport") && w.Message.Contains("ignored"));
    }

    [Fact]
    public void Unknown_parameter_is_reported_not_swallowed()
    {
        var report = CrystalReport.Load(SampleBytes("param_kind_discrete.rpt"));
        report.SetParameterValue("NotAParam", "x");
        report.Export();
        Assert.Contains(report.Warnings, w => w.Message.Contains("NotAParam"));
    }

    [Fact]
    public void Stream_overload_writes_the_same_bytes()
    {
        var report = CrystalReport.Load(SampleBytes("sports_teams.rpt"));
        var expected = report.ExportToPdf();
        using var target = new MemoryStream();
        report.ExportToPdf(target);
        Assert.Equal(expected, target.ToArray());
    }

    [Fact]
    public void Malformed_bytes_fail_with_a_decode_error()
    {
        var exception = Assert.Throws<RptCsException>(() => CrystalReport.Load(new byte[] { 1, 2, 3, 4 }));
        Assert.Equal(2, exception.Code);
    }

    [Fact]
    public void Native_version_reports_engine_provenance()
    {
        Assert.Contains("rpt-rs", Native.Version());
    }
}