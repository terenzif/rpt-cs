using System;
using System.Collections.Generic;
using System.Data;
using System.IO;

using RptCs;

namespace RptCs.Compat
{
    /// <summary>
    /// Compile-time contract: the API shapes a .NET Framework 4.8 / C# 7.3 consumer relies on to
    /// render Crystal Reports .rpt files to PDF without the Crystal Reports runtime. The project
    /// has no tests of its own — building it under this TFM and language level IS the check, and
    /// any breaking change to the public surface fails this build.
    /// </summary>
    internal static class ClientPatterns
    {
        /// <summary>Push model: bind a multi-table DataSet, set parameters, export the PDF.</summary>
        internal static byte[] RenderFromDataSet(string reportPath, DataSet dataSet, string parameterName, int parameterValue)
        {
            var report = CrystalReport.Load(reportPath);
            report.SetDataSource(dataSet);
            report.SetParameterValue(parameterName, parameterValue);
            using (var pdfStream = report.ExportToStream())
            using (var output = new MemoryStream())
            {
                pdfStream.CopyTo(output);
                return output.ToArray();
            }
        }

        /// <summary>Single-table live feed, subreport rows keyed by name.</summary>
        internal static byte[] RenderWithSubreport(string reportPath, DataTable mainTable, string subreportName, DataTable subreportTable)
        {
            var report = CrystalReport.Load(reportPath);
            report.SetDataSource(mainTable);
            report.SetSubreportData(subreportName, subreportTable);
            return report.ExportToPdf();
        }

        /// <summary>Metadata introspection: which fields does the report reference.</summary>
        internal static string[] RequiredFields(CrystalReport report)
        {
            var names = new List<string>();
            foreach (var field in report.Info.Fields)
            {
                names.Add(field.QualifiedName);
            }
            return names.ToArray();
        }

        /// <summary>Diagnostics survive the export and stay inspectable.</summary>
        internal static int CountWarnings(CrystalReport report)
        {
            var count = 0;
            foreach (var warning in report.Warnings)
            {
                if (warning.Severity == "warning")
                {
                    count++;
                }
            }
            return count;
        }

        /// <summary>Metadata: parameters, subreports, tables, saved data.</summary>
        internal static string FirstSubreportName(CrystalReport report)
        {
            return report.Info.Subreports.Count > 0 ? report.Info.Subreports[0] : "";
        }

        /// <summary>Plain PDF export with options.</summary>
        internal static byte[] ExportWithConformance(CrystalReport report)
        {
            return report.ExportToPdf(new PdfExportOptions
            {
                Conformance = PdfConformance.PdfA2b,
                Locale = "it-IT",
                AsOf = new DateTime(2026, 1, 1),
            });
        }
    }
}