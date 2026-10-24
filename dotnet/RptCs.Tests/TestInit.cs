using System;
using System.IO;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace RptCs.Tests;

/// <summary>Points the native resolver at the locally built rpt_cs binary before any P/Invoke fires.</summary>
internal static class TestInit
{
    [ModuleInitializer]
    internal static void Initialize()
    {
        if (!string.IsNullOrEmpty(Environment.GetEnvironmentVariable("RPT_CS_NATIVE_PATH")))
        {
            return;
        }
        var name = NativeName();
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir != null; dir = dir.Parent)
        {
            foreach (var profile in new[] { "release", "debug" })
            {
                var candidate = Path.Combine(dir.FullName, "native", "target", profile, name);
                if (File.Exists(candidate))
                {
                    Environment.SetEnvironmentVariable("RPT_CS_NATIVE_PATH", candidate);
                    return;
                }
            }
        }
    }

    private static string NativeName()
    {
        if (OperatingSystem.IsWindows())
        {
            return "rpt_cs.dll";
        }
        if (OperatingSystem.IsMacOS())
        {
            return "librpt_cs.dylib";
        }
        return "librpt_cs.so";
    }
}