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
        var triple = HostTriple();
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir != null; dir = dir.Parent)
        {
            foreach (var profile in new[] { "release", "debug" })
            {
                foreach (var candidate in new[]
                {
                    Path.Combine(dir.FullName, "native", "target", triple, profile, name),
                    Path.Combine(dir.FullName, "native", "target", profile, name),
                })
                {
                    if (File.Exists(candidate))
                    {
                        Environment.SetEnvironmentVariable("RPT_CS_NATIVE_PATH", candidate);
                        return;
                    }
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

    private static string HostTriple()
    {
        var arch = RuntimeInformation.ProcessArchitecture;
        if (OperatingSystem.IsWindows())
        {
            return arch switch
            {
                Architecture.Arm64 => "aarch64-pc-windows-msvc",
                Architecture.X86 => "i686-pc-windows-msvc",
                _ => "x86_64-pc-windows-msvc",
            };
        }
        if (OperatingSystem.IsMacOS())
        {
            return arch == Architecture.Arm64 ? "aarch64-apple-darwin" : "x86_64-apple-darwin";
        }
        return arch switch
        {
            Architecture.Arm64 => "aarch64-unknown-linux-gnu",
            Architecture.X86 => "i686-unknown-linux-gnu",
            _ => "x86_64-unknown-linux-gnu",
        };
    }
}