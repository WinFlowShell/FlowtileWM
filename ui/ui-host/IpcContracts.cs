using System.Text.Json;

namespace Flowtile.UiHost;

public sealed class IpcEventEnvelope
{
    public uint ProtocolVersion { get; set; }

    public ulong StreamVersion { get; set; }

    public string EventId { get; set; } = string.Empty;

    public string EventKind { get; set; } = string.Empty;

    public ulong StateVersion { get; set; }

    public JsonElement Payload { get; set; }

    public ulong Timestamp { get; set; }
}

public sealed class IpcRequestEnvelope
{
    public uint ProtocolVersion { get; set; } = 1;

    public string RequestId { get; set; } = string.Empty;

    public string Command { get; set; } = string.Empty;

    public object Payload { get; set; } = new { };
}

public sealed class IpcResponseEnvelope
{
    public uint ProtocolVersion { get; set; }

    public string RequestId { get; set; } = string.Empty;

    public bool Ok { get; set; }

    public JsonElement Result { get; set; }

    public IpcErrorEnvelope? Error { get; set; }
}

public sealed class IpcErrorEnvelope
{
    public string Code { get; set; } = string.Empty;

    public string Message { get; set; } = string.Empty;

    public string Category { get; set; } = string.Empty;

    public bool Retryable { get; set; }

    public JsonElement Details { get; set; }
}

public sealed class SnapshotProjection
{
    public string VersionLine { get; set; } = string.Empty;

    public string RuntimeMode { get; set; } = string.Empty;

    public ulong StateVersion { get; set; }

    public List<OutputProjection> Outputs { get; set; } = [];

    public List<WorkspaceProjection> Workspaces { get; set; } = [];

    public List<WindowProjection> Windows { get; set; } = [];

    public FocusProjection Focus { get; set; } = new();

    public OverviewProjection Overview { get; set; } = new();

    public DiagnosticsProjection Diagnostics { get; set; } = new();

    public ConfigProjection Config { get; set; } = new();
}

public sealed class RectProjection
{
    public int X { get; set; }

    public int Y { get; set; }

    public uint Width { get; set; }

    public uint Height { get; set; }
}

public sealed class OutputProjection
{
    public ulong MonitorId { get; set; }

    public string? Binding { get; set; }

    public uint Dpi { get; set; }

    public bool IsPrimary { get; set; }

    public RectProjection WorkArea { get; set; } = new();

    public int WorkspaceCount { get; set; }

    public ulong? ActiveWorkspaceId { get; set; }
}

public sealed class WorkspaceProjection
{
    public ulong WorkspaceId { get; set; }

    public ulong MonitorId { get; set; }

    public int VerticalIndex { get; set; }

    public string? Name { get; set; }

    public bool IsActive { get; set; }

    public bool IsEmpty { get; set; }

    public bool IsTail { get; set; }

    public int ScrollOffset { get; set; }

    public int ColumnCount { get; set; }

    public int TiledWindowCount { get; set; }

    public int FloatingWindowCount { get; set; }
}

public sealed class WindowProjection
{
    public ulong WindowId { get; set; }

    public ulong MonitorId { get; set; }

    public ulong WorkspaceId { get; set; }

    public ulong? ColumnId { get; set; }

    public ulong? Hwnd { get; set; }

    public string Title { get; set; } = string.Empty;

    public string ClassName { get; set; } = string.Empty;

    public string? ProcessName { get; set; }

    public string Layer { get; set; } = string.Empty;

    public string Classification { get; set; } = string.Empty;

    public bool IsManaged { get; set; }

    public bool IsFocused { get; set; }
}

public sealed class FocusProjection
{
    public ulong? MonitorId { get; set; }

    public ulong? WorkspaceId { get; set; }

    public ulong? ColumnId { get; set; }

    public ulong? WindowId { get; set; }

    public string Origin { get; set; } = string.Empty;
}

public sealed class OverviewProjection
{
    public bool IsOpen { get; set; }

    public ulong? MonitorId { get; set; }

    public ulong? SelectionWorkspaceId { get; set; }

    public ulong ProjectionVersion { get; set; }
}

public sealed class DiagnosticsProjection
{
    public ulong TotalRecords { get; set; }

    public string? LastTransitionLabel { get; set; }

    public List<string> DegradedFlags { get; set; } = [];

    public bool ManagementEnabled { get; set; }
}

public sealed class ConfigProjection
{
    public ulong ConfigVersion { get; set; }

    public string SourcePath { get; set; } = string.Empty;

    public int ActiveRuleCount { get; set; }

    public uint StripScrollStep { get; set; }

    public string DefaultColumnMode { get; set; } = string.Empty;
}

public sealed class OutputsDelta
{
    public List<OutputProjection> Outputs { get; set; } = [];
}

public sealed class WorkspacesDelta
{
    public List<WorkspaceProjection> Workspaces { get; set; } = [];
}

public sealed class WindowsDelta
{
    public List<WindowProjection> Windows { get; set; } = [];
}

public sealed class FocusDelta
{
    public FocusProjection Focus { get; set; } = new();
}

public sealed class OverviewDelta
{
    public OverviewProjection Overview { get; set; } = new();
}

public sealed class DiagnosticsDelta
{
    public DiagnosticsProjection Diagnostics { get; set; } = new();
}

public sealed class ConfigDelta
{
    public ConfigProjection Config { get; set; } = new();
}
