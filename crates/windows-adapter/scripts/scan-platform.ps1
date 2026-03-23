Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class FlowtileNative
{
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    public delegate bool EnumDisplayMonitorsProc(IntPtr hMonitor, IntPtr hdc, IntPtr lprcMonitor, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct MONITORINFOEX
    {
        public int cbSize;
        public RECT rcMonitor;
        public RECT rcWork;
        public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]
        public string szDevice;
    }

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumDisplayMonitors(
        IntPtr hdc,
        IntPtr lprcClip,
        EnumDisplayMonitorsProc lpfnEnum,
        IntPtr dwData);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hWnd, StringBuilder lpClassName, int nMaxCount);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    public static extern IntPtr GetShellWindow();

    [DllImport("user32.dll")]
    public static extern IntPtr GetWindow(IntPtr hWnd, uint uCmd);

    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    public static extern IntPtr GetWindowLongPtr(IntPtr hWnd, int nIndex);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern bool GetMonitorInfo(IntPtr hMonitor, ref MONITORINFOEX lpmi);

    [DllImport("user32.dll")]
    public static extern IntPtr MonitorFromWindow(IntPtr hWnd, uint dwFlags);

    [DllImport("Shcore.dll")]
    public static extern int GetDpiForMonitor(IntPtr hmonitor, int dpiType, out uint dpiX, out uint dpiY);

    [DllImport("dwmapi.dll")]
    public static extern int DwmGetWindowAttribute(IntPtr hwnd, int dwAttribute, out int pvAttribute, int cbAttribute);
}
"@

$DWMWA_CLOAKED = 14

function Convert-Rect {
    param(
        [FlowtileNative+RECT]$Rect
    )

    return @{
        x = [int]$Rect.Left
        y = [int]$Rect.Top
        width = [Math]::Max(0, [int]($Rect.Right - $Rect.Left))
        height = [Math]::Max(0, [int]($Rect.Bottom - $Rect.Top))
    }
}

function Get-MonitorObject {
    param(
        [IntPtr]$MonitorHandle
    )

    if ($MonitorHandle -eq [IntPtr]::Zero) {
        return $null
    }

    $info = New-Object FlowtileNative+MONITORINFOEX
    $info.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][FlowtileNative+MONITORINFOEX])
    if (-not [FlowtileNative]::GetMonitorInfo($MonitorHandle, [ref]$info)) {
        return $null
    }

    $dpiX = [uint32]96
    $dpiY = [uint32]96
    [void][FlowtileNative]::GetDpiForMonitor($MonitorHandle, 0, [ref]$dpiX, [ref]$dpiY)

    return @{
        binding = $info.szDevice
        work_area_rect = Convert-Rect -Rect $info.rcWork
        dpi = [int]$dpiX
        is_primary = (($info.dwFlags -band 1) -ne 0)
    }
}

function Test-IsCloakedWindow {
    param(
        [IntPtr]$WindowHandle
    )

    $cloaked = 0
    $result = [FlowtileNative]::DwmGetWindowAttribute(
        $WindowHandle,
        $DWMWA_CLOAKED,
        [ref]$cloaked,
        4
    )

    return ($result -eq 0) -and ($cloaked -ne 0)
}

try {
    $monitors = [System.Collections.Generic.List[object]]::new()
    $monitorBindings = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $monitorCallback = [FlowtileNative+EnumDisplayMonitorsProc]{
        param(
            [IntPtr]$MonitorHandle,
            [IntPtr]$DeviceContext,
            [IntPtr]$MonitorRect,
            [IntPtr]$UserData
        )

        $monitor = Get-MonitorObject -MonitorHandle $MonitorHandle
        if ($null -ne $monitor -and $monitorBindings.Add($monitor.binding)) {
            [void]$monitors.Add($monitor)
        }

        return $true
    }
    [void][FlowtileNative]::EnumDisplayMonitors(
        [IntPtr]::Zero,
        [IntPtr]::Zero,
        $monitorCallback,
        [IntPtr]::Zero
    )

    $foregroundHandle = [uint64][FlowtileNative]::GetForegroundWindow().ToInt64()
    $shellHandle = [uint64][FlowtileNative]::GetShellWindow().ToInt64()
    $windows = [System.Collections.Generic.List[object]]::new()
    $windowCallback = [FlowtileNative+EnumWindowsProc]{
        param(
            [IntPtr]$WindowHandle,
            [IntPtr]$UserData
        )

        $hwnd = [uint64]$WindowHandle.ToInt64()
        if ($hwnd -eq 0 -or $hwnd -eq $shellHandle) {
            return $true
        }

        if (-not [FlowtileNative]::IsWindowVisible($WindowHandle)) {
            return $true
        }

        if ([FlowtileNative]::IsIconic($WindowHandle)) {
            return $true
        }

        if (Test-IsCloakedWindow -WindowHandle $WindowHandle) {
            return $true
        }

        $owner = [FlowtileNative]::GetWindow($WindowHandle, 4)
        if ($owner -ne [IntPtr]::Zero) {
            return $true
        }

        $exStyle = [uint64][FlowtileNative]::GetWindowLongPtr($WindowHandle, -20).ToInt64()
        if (($exStyle -band 0x80) -ne 0) {
            return $true
        }

        $windowRect = New-Object FlowtileNative+RECT
        if (-not [FlowtileNative]::GetWindowRect($WindowHandle, [ref]$windowRect)) {
            return $true
        }

        $rect = Convert-Rect -Rect $windowRect
        if ($rect.width -le 0 -or $rect.height -le 0) {
            return $true
        }

        $titleBuilder = New-Object System.Text.StringBuilder 512
        [void][FlowtileNative]::GetWindowText($WindowHandle, $titleBuilder, $titleBuilder.Capacity)
        $classBuilder = New-Object System.Text.StringBuilder 256
        [void][FlowtileNative]::GetClassName($WindowHandle, $classBuilder, $classBuilder.Capacity)
        $className = $classBuilder.ToString()

        if ($className -eq 'Windows.UI.Core.CoreWindow') {
            return $true
        }

        $processId = [uint32]0
        [void][FlowtileNative]::GetWindowThreadProcessId($WindowHandle, [ref]$processId)
        $processName = $null
        if ($processId -ne 0) {
            try {
                $processName = (Get-Process -Id $processId -ErrorAction Stop).ProcessName
            }
            catch {
                $processName = $null
            }
        }

        $monitorHandle = [FlowtileNative]::MonitorFromWindow($WindowHandle, 2)
        $monitor = Get-MonitorObject -MonitorHandle $monitorHandle
        if ($null -eq $monitor) {
            return $true
        }

        [void]$windows.Add(@{
            hwnd = $hwnd
            title = $titleBuilder.ToString()
            class_name = $className
            process_id = [int]$processId
            process_name = $processName
            rect = $rect
            monitor_binding = $monitor.binding
            is_visible = $true
            is_focused = ($hwnd -eq $foregroundHandle)
        })

        return $true
    }
    [void][FlowtileNative]::EnumWindows($windowCallback, [IntPtr]::Zero)

    @{
        monitors = $monitors
        windows = $windows
    } | ConvertTo-Json -Depth 8 -Compress
}
catch {
    Write-Error $_
    exit 1
}
