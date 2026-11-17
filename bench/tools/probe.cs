// probe.exe - batch process runner that records wall time + peak working set.
//
// Usage:
//   probe --manifest cases.json --out out.jsonl [--reps 30] [--logdir dir]
//         [--env K=V ...] [--timeout 600000]
//
// cases.json: [{"id":"...","exe":"...","args":["..."],"cwd":"...","timeoutMs":60000}]
//
// For every repetition it runs every case in round-robin order (interleaved, so
// machine drift hits all cases equally), and appends one JSON line:
//   {"id":"..","rep":0,"wall_ms":12.34,"peak_kb":1234,"exit":0,"timedOut":false,"log":".."}
// Peak working set is read with GetProcessMemoryInfo while our handle to the
// (already exited) child is still open -- that is why we CreateProcess ourselves
// instead of letting Start-Process Release() the process object.

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

static class Probe
{
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION
    {
        public IntPtr hProcess, hThread;
        public int dwProcessId, dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_MEMORY_COUNTERS
    {
        public uint cb;
        public uint PageFaultCount;
        public UIntPtr PeakWorkingSetSize;
        public UIntPtr WorkingSetSize;
        public UIntPtr QuotaPeakPagedPoolUsage;
        public UIntPtr QuotaPagedPoolUsage;
        public UIntPtr QuotaPeakNonPagedPoolUsage;
        public UIntPtr QuotaNonPagedPoolUsage;
        public UIntPtr PagefileUsage;
        public UIntPtr PeakPagefileUsage;
    }

    const uint GENERIC_WRITE = 0x40000000;
    const uint FILE_SHARE_READ = 1;
    const uint CREATE_ALWAYS = 2;
    const uint FILE_ATTRIBUTE_NORMAL = 0x80;
    const int STARTF_USESTDHANDLES = 0x100;
    const uint CREATE_NO_WINDOW = 0x08000000;
    const uint WAIT_TIMEOUT = 0x102;

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcess(string lpApplicationName, string lpCommandLine,
        IntPtr lpProcessAttributes, IntPtr lpThreadAttributes, bool bInheritHandles,
        uint dwCreationFlags, IntPtr lpEnvironment, string lpCurrentDirectory,
        ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr CreateFile(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sec,
        uint disp, uint flags, IntPtr tmpl);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr h, uint ms);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr h, out uint code);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr h, uint code);

    [DllImport("psapi.dll", SetLastError = true)]
    static extern bool GetProcessMemoryInfo(IntPtr h, out PROCESS_MEMORY_COUNTERS c, uint cb);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr h);

    class Case
    {
        public string id, exe, cwd;
        public string[] args = new string[0];
        public int timeoutMs = 600000;
        public string logPath;
    }

    static string JsonEsc(string s)
    {
        var sb = new StringBuilder();
        foreach (char c in s)
        {
            switch (c)
            {
                case '"': sb.Append("\\\""); break;
                case '\\': sb.Append("\\\\"); break;
                case '\n': sb.Append("\\n"); break;
                case '\r': sb.Append("\\r"); break;
                case '\t': sb.Append("\\t"); break;
                default:
                    if (c < 0x20) sb.AppendFormat("\\u{0:x4}", (int)c);
                    else sb.Append(c);
                    break;
            }
        }
        return sb.ToString();
    }

    static string Quote(string a)
    {
        if (a.Length > 0 && a.IndexOfAny(new char[] { ' ', '\t', '"' }) < 0) return a;
        var sb = new StringBuilder("\"");
        foreach (char c in a)
        {
            if (c == '"') sb.Append('\\');
            sb.Append(c);
        }
        sb.Append('"');
        return sb.ToString();
    }

    // ---- tiny JSON reader (array of flat objects with string/array/number values)
    static List<Dictionary<string, object>> ReadCases(string path)
    {
        string text = File.ReadAllText(path);
        var res = new List<Dictionary<string, object>>();
        int i = 0;
        while (i < text.Length && text[i] != '[') i++;
        i++; // skip [
        while (i < text.Length)
        {
            while (i < text.Length && (char.IsWhiteSpace(text[i]) || text[i] == ',')) i++;
            if (i >= text.Length || text[i] == ']') break;
            if (text[i] != '{') { i++; continue; }
            i++;
            var obj = new Dictionary<string, object>();
            while (i < text.Length && text[i] != '}')
            {
                while (i < text.Length && (char.IsWhiteSpace(text[i]) || text[i] == ',')) i++;
                if (text[i] == '}') break;
                // key
                if (text[i] != '"') { i++; continue; }
                int ks = ++i;
                while (i < text.Length && text[i] != '"') { if (text[i] == '\\') i++; i++; }
                string key = text.Substring(ks, i - ks);
                i++; // closing quote
                while (i < text.Length && (char.IsWhiteSpace(text[i]) || text[i] == ':')) i++;
                if (text[i] == '"')
                {
                    int vs = ++i;
                    var sb = new StringBuilder();
                    while (i < text.Length && text[i] != '"')
                    {
                        if (text[i] == '\\' && i + 1 < text.Length)
                        {
                            i++;
                            switch (text[i])
                            {
                                case 'n': sb.Append('\n'); break;
                                case 't': sb.Append('\t'); break;
                                case 'r': sb.Append('\r'); break;
                                case '\\': sb.Append('\\'); break;
                                case '"': sb.Append('"'); break;
                                default: sb.Append(text[i]); break;
                            }
                            i++;
                        }
                        else sb.Append(text[i++]);
                    }
                    obj[key] = sb.ToString();
                    i++;
                }
                else if (text[i] == '[')
                {
                    i++;
                    var list = new List<string>();
                    while (i < text.Length && text[i] != ']')
                    {
                        if (text[i] == '"')
                        {
                            i++;
                            var sb = new StringBuilder();
                            while (i < text.Length && text[i] != '"')
                            {
                                if (text[i] == '\\' && i + 1 < text.Length) { i++; sb.Append(text[i]); i++; }
                                else sb.Append(text[i++]);
                            }
                            i++;
                            list.Add(sb.ToString());
                        }
                        else i++;
                    }
                    i++; // ]
                    obj[key] = list;
                }
                else
                {
                    int ns = i;
                    while (i < text.Length && (char.IsDigit(text[i]) || text[i] == '-' || text[i] == '.' || text[i] == 'e' || text[i] == 'E' || text[i] == '+')) i++;
                    double d;
                    double.TryParse(text.Substring(ns, i - ns), NumberStyles.Float, CultureInfo.InvariantCulture, out d);
                    obj[key] = d;
                }
            }
            i++; // }
            res.Add(obj);
        }
        return res;
    }

    static int Main(string[] argv)
    {
        string manifest = null, outPath = null, logdir = null;
        int reps = 30, timeout = 600000;
        var envs = new Dictionary<string, string>();

        for (int i = 0; i < argv.Length; i++)
        {
            switch (argv[i])
            {
                case "--manifest": manifest = argv[++i]; break;
                case "--out": outPath = argv[++i]; break;
                case "--reps": reps = int.Parse(argv[++i]); break;
                case "--logdir": logdir = argv[++i]; break;
                case "--timeout": timeout = int.Parse(argv[++i]); break;
                case "--env":
                    {
                        string kv = argv[++i];
                        int eq = kv.IndexOf('=');
                        envs[kv.Substring(0, eq)] = kv.Substring(eq + 1);
                        break;
                    }
                default:
                    Console.Error.WriteLine("probe: unknown arg " + argv[i]);
                    return 2;
            }
        }
        if (manifest == null || outPath == null) { Console.Error.WriteLine("probe: --manifest and --out required"); return 2; }
        if (logdir != null) Directory.CreateDirectory(logdir);

        foreach (var kv in envs) Environment.SetEnvironmentVariable(kv.Key, kv.Value);

        var raw = ReadCases(manifest);
        var cases = new List<Case>();
        foreach (var o in raw)
        {
            var c = new Case();
            c.id = (string)o["id"];
            c.exe = (string)o["exe"];
            if (o.ContainsKey("cwd")) c.cwd = (string)o["cwd"];
            if (o.ContainsKey("args")) c.args = ((List<string>)o["args"]).ToArray();
            if (o.ContainsKey("timeoutMs")) c.timeoutMs = (int)(double)o["timeoutMs"];
            if (c.timeoutMs <= 0) c.timeoutMs = timeout;
            if (logdir != null)
            {
                string safe = c.id.Replace('/', '_').Replace('\\', '_').Replace(':', '_').Replace(' ', '_');
                c.logPath = Path.Combine(logdir, safe + ".log");
            }
            cases.Add(c);
        }

        var inv = CultureInfo.InvariantCulture;
        using (var sw = new StreamWriter(outPath, false, new UTF8Encoding(false)))
        {
            for (int rep = 0; rep < reps; rep++)
            {
                foreach (var c in cases)
                {
                    // ---- child stdio: NUL unless a log path is given
                    string target = c.logPath ?? "NUL";
                    var sa = new SECURITY_ATTRIBUTES();
                    sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
                    sa.bInheritHandle = true;   // 不设它，子进程拿到的句柄无效 -> 日志全丢
                    IntPtr hOut = CreateFile(target, GENERIC_WRITE, FILE_SHARE_READ, ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
                    var si = new STARTUPINFO();
                    si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
                    si.dwFlags = STARTF_USESTDHANDLES;
                    si.hStdOutput = hOut;
                    si.hStdError = hOut;
                    si.hStdInput = IntPtr.Zero;

                    var cmd = new StringBuilder();
                    cmd.Append(Quote(c.exe));
                    foreach (var a in c.args) { cmd.Append(' '); cmd.Append(Quote(a)); }

                    PROCESS_INFORMATION pi;
                    var sw2 = Stopwatch.StartNew();
                    bool ok = CreateProcess(c.exe, cmd.ToString(), IntPtr.Zero, IntPtr.Zero, true,
                        CREATE_NO_WINDOW, IntPtr.Zero, c.cwd, ref si, out pi);
                    if (!ok)
                    {
                        sw2.Stop();
                        if (hOut != IntPtr.Zero) CloseHandle(hOut);
                        sw.WriteLine("{\"id\":\"" + JsonEsc(c.id) + "\",\"rep\":" + rep +
                                     ",\"error\":\"CreateProcess failed " + Marshal.GetLastWin32Error() + "\"}");
                        continue;
                    }
                    uint wr = WaitForSingleObject(pi.hProcess, (uint)c.timeoutMs);
                    sw2.Stop();
                    bool timedOut = wr == WAIT_TIMEOUT;
                    uint code = 0;
                    if (timedOut) { TerminateProcess(pi.hProcess, 99); WaitForSingleObject(pi.hProcess, 5000); }
                    GetExitCodeProcess(pi.hProcess, out code);

                    PROCESS_MEMORY_COUNTERS pmc;
                    pmc.cb = (uint)Marshal.SizeOf(typeof(PROCESS_MEMORY_COUNTERS));
                    bool memok = GetProcessMemoryInfo(pi.hProcess, out pmc, pmc.cb);

                    // thread/CPU time is not needed; free handles
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    if (hOut != IntPtr.Zero) CloseHandle(hOut);

                    double peakKb = memok ? (double)pmc.PeakWorkingSetSize.ToUInt64() / 1024.0 : -1;
                    sw.WriteLine("{\"id\":\"" + JsonEsc(c.id) + "\",\"rep\":" + rep +
                                 ",\"wall_ms\":" + sw2.Elapsed.TotalMilliseconds.ToString("F3", inv) +
                                 ",\"peak_kb\":" + peakKb.ToString("F1", inv) +
                                 ",\"exit\":" + code +
                                 ",\"timedOut\":" + (timedOut ? "true" : "false") + "}");
                    sw.Flush();
                    if (timedOut) Console.Error.WriteLine("probe: timeout " + c.id);
                }
            }
        }
        return 0;
    }
}
