using System;
using System.IO;
using System.Runtime.InteropServices;

namespace RptCs;

/// <summary>Failure raised by the native engine, carrying its ABI code.</summary>
public class RptCsException : Exception
{
    /// <summary>The ABI code: 1 invalid argument, 2 decode error, 3 PDF error, 4 internal error.</summary>
    public int Code { get; }

    public RptCsException(int code, string message) : base(message) => Code = code;
}

/// <summary>P/Invoke surface over the <c>rpt_cs</c> native library, with a per-OS asset resolver.</summary>
internal static class Native
{
    private const string LibraryName = "rpt_cs";

    [StructLayout(LayoutKind.Sequential)]
    private struct CreResult
    {
        public int Code;
        public IntPtr Pdf;
        public UIntPtr PdfLen;
        public IntPtr Json;
        public UIntPtr JsonLen;
    }

    static Native() => TryAttachResolver();

#if NET5_0_OR_GREATER
    private static void TryAttachResolver()
    {
        try
        {
            NativeLibrary.SetDllImportResolver(typeof(Native).Assembly, Resolve);
        }
        catch (InvalidOperationException)
        {
            // A resolver is already attached; the default search path still applies afterwards.
        }
        catch (MissingMethodException)
        {
            // Host without NativeLibrary support; fall back to default probing.
        }
    }

    private static IntPtr Resolve(string libraryName, System.Reflection.Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (libraryName != LibraryName)
        {
            return IntPtr.Zero;
        }

        // Dev override: RPT_CS_NATIVE_PATH points at a locally built rpt_cs binary.
        var env = Environment.GetEnvironmentVariable("RPT_CS_NATIVE_PATH");
        if (!string.IsNullOrEmpty(env) && NativeLibrary.TryLoad(env, out var fromEnv))
        {
            return fromEnv;
        }

        var fileName = NativeFileName();
        var rid = RuntimeIdentifier();
        var bases = new[]
        {
            AppContext.BaseDirectory,
            Path.GetDirectoryName(assembly.Location),
        };
        foreach (var b in bases)
        {
            if (string.IsNullOrEmpty(b))
            {
                continue;
            }
            if (NativeLibrary.TryLoad(Path.Combine(b, "runtimes", rid, "native", fileName), out var handle) ||
                NativeLibrary.TryLoad(Path.Combine(b, fileName), out handle))
            {
                return handle;
            }
        }
        return IntPtr.Zero; // fall through to the default probing order
    }

    private static string NativeFileName()
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

    private static string RuntimeIdentifier()
    {
        var arch = RuntimeInformation.ProcessArchitecture switch
        {
            Architecture.Arm64 => "arm64",
            Architecture.Arm => "arm",
            Architecture.X86 => "x86",
            _ => "x64",
        };
        var os = OperatingSystem.IsWindows() ? "win"
            : OperatingSystem.IsMacOS() ? "osx"
            : "linux";
        return $"{os}-{arch}";
    }
#else
    private static void TryAttachResolver()
    {
        // netstandard2.0: default P/Invoke probing (application directory / PATH).
        // Place rpt_cs.dll|librpt_cs.so|librpt_cs.dylib next to the managed assembly.
    }
#endif

    [DllImport(LibraryName, EntryPoint = "cre_version")]
    private static extern IntPtr CreVersion();

    [DllImport(LibraryName, EntryPoint = "cre_inspect")]
    private static extern IntPtr CreInspect(byte[] bytes, UIntPtr len);

    [DllImport(LibraryName, EntryPoint = "cre_render")]
    private static extern IntPtr CreRender(byte[] bytes, UIntPtr len, byte[] request, UIntPtr reqLen);

    [DllImport(LibraryName, EntryPoint = "cre_result_free")]
    private static extern void CreResultFree(IntPtr result);

    internal static string Version()
    {
        return Marshal.PtrToStringAnsi(CreVersion()) ?? "";
    }

    /// <summary>One call's decoded outcome: code plus the PDF and JSON buffers.</summary>
    internal sealed class Result
    {
        public int Code;
        public byte[] Pdf = Array.Empty<byte>();
        public byte[] Json = Array.Empty<byte>();
    }

    internal static Result Inspect(byte[] rpt)
    {
        var ptr = CreInspect(rpt, (UIntPtr)rpt.Length);
        return ReadAndFree(ptr);
    }

    internal static Result Render(byte[] rpt, byte[] request)
    {
        var ptr = CreRender(rpt, (UIntPtr)rpt.Length, request, (UIntPtr)request.Length);
        return ReadAndFree(ptr);
    }

    private static Result ReadAndFree(IntPtr result)
    {
        if (result == IntPtr.Zero)
        {
            throw new RptCsException(4, "native call returned no result");
        }
        try
        {
            var r = Marshal.PtrToStructure<CreResult>(result);
            var copy = new Result { Code = r.Code };
            copy.Pdf = CopyBuffer(r.Pdf, r.PdfLen);
            copy.Json = CopyBuffer(r.Json, r.JsonLen);
            return copy;
        }
        finally
        {
            CreResultFree(result);
        }
    }

    private static byte[] CopyBuffer(IntPtr src, UIntPtr len)
    {
        if (src == IntPtr.Zero || len == UIntPtr.Zero)
        {
            return Array.Empty<byte>();
        }
        var count = checked((int)(uint)len);
        var buffer = new byte[count];
        Marshal.Copy(src, buffer, 0, count);
        return buffer;
    }
}