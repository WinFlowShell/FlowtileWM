using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;

namespace Flowtile.UiHost;

public sealed class FlowtileShellState : INotifyPropertyChanged
{
    private string connectionState = "Disconnected";
    private string connectionDetail = @"Waiting for \\.\pipe\flowtilewm-event-stream-v1";
    private string lastError = "No transport errors.";
    private string lastEvent = "No events received yet.";
    private string lastCommand = "No UI commands sent yet.";
    private string versionLine = "unknown";
    private string runtimeMode = "unknown";
    private ulong stateVersion;
    private string focusSummary = "Focus projection is not available yet.";
    private string overviewSummary = "Overview is closed.";
    private string diagnosticsSummary = "Diagnostics projection is not available yet.";
    private string configSummary = "Config projection is not available yet.";
    private string outputSummary = "No outputs projected.";
    private string workspaceSummary = "No workspaces projected.";
    private string windowSummary = "No windows projected.";
    private bool overviewOpen;

    public event PropertyChangedEventHandler? PropertyChanged;

    public ObservableCollection<OutputCard> Outputs { get; } = [];

    public ObservableCollection<WorkspaceCard> Workspaces { get; } = [];

    public ObservableCollection<WindowCard> Windows { get; } = [];

    public string ConnectionState
    {
        get => connectionState;
        private set => SetField(ref connectionState, value);
    }

    public string ConnectionDetail
    {
        get => connectionDetail;
        private set => SetField(ref connectionDetail, value);
    }

    public string LastError
    {
        get => lastError;
        private set => SetField(ref lastError, value);
    }

    public string LastEvent
    {
        get => lastEvent;
        private set => SetField(ref lastEvent, value);
    }

    public string LastCommand
    {
        get => lastCommand;
        private set => SetField(ref lastCommand, value);
    }

    public string VersionLine
    {
        get => versionLine;
        private set => SetField(ref versionLine, value);
    }

    public string RuntimeMode
    {
        get => runtimeMode;
        private set => SetField(ref runtimeMode, value);
    }

    public ulong StateVersion
    {
        get => stateVersion;
        private set => SetField(ref stateVersion, value);
    }

    public string FocusSummary
    {
        get => focusSummary;
        private set => SetField(ref focusSummary, value);
    }

    public string OverviewSummary
    {
        get => overviewSummary;
        private set => SetField(ref overviewSummary, value);
    }

    public string DiagnosticsSummary
    {
        get => diagnosticsSummary;
        private set => SetField(ref diagnosticsSummary, value);
    }

    public string ConfigSummary
    {
        get => configSummary;
        private set => SetField(ref configSummary, value);
    }

    public string OutputSummary
    {
        get => outputSummary;
        private set => SetField(ref outputSummary, value);
    }

    public string WorkspaceSummary
    {
        get => workspaceSummary;
        private set => SetField(ref workspaceSummary, value);
    }

    public string WindowSummary
    {
        get => windowSummary;
        private set => SetField(ref windowSummary, value);
    }

    public bool OverviewOpen
    {
        get => overviewOpen;
        private set => SetField(ref overviewOpen, value);
    }

    public bool IsConnected => ConnectionState == "Connected";

    public string ConnectionBadge => IsConnected ? "Daemon connected" : "Daemon disconnected";

    public void MarkConnecting()
    {
        ConnectionState = "Connecting";
        ConnectionDetail = @"Connecting to \\.\pipe\flowtilewm-event-stream-v1";
        OnPropertyChanged(nameof(IsConnected));
        OnPropertyChanged(nameof(ConnectionBadge));
    }

    public void MarkConnected()
    {
        ConnectionState = "Connected";
        ConnectionDetail = @"Streaming projections from \\.\pipe\flowtilewm-event-stream-v1";
        LastError = "No transport errors.";
        OnPropertyChanged(nameof(IsConnected));
        OnPropertyChanged(nameof(ConnectionBadge));
    }

    public void MarkDisconnected(string detail)
    {
        ConnectionState = "Disconnected";
        ConnectionDetail = @"Reconnect is waiting for \\.\pipe\flowtilewm-event-stream-v1";
        LastError = detail;
        OnPropertyChanged(nameof(IsConnected));
        OnPropertyChanged(nameof(ConnectionBadge));
    }

    public void MarkCommandResult(string label)
    {
        LastCommand = label;
    }

    public void RecordEvent(string eventKind, ulong eventStateVersion)
    {
        LastEvent = $"{eventKind} @ state {eventStateVersion}";
    }

    public void ApplySnapshot(SnapshotProjection snapshot)
    {
        VersionLine = snapshot.VersionLine;
        RuntimeMode = snapshot.RuntimeMode;
        StateVersion = snapshot.StateVersion;
        ReplaceWith(Outputs, snapshot.Outputs.Select(MapOutput));
        ReplaceWith(Workspaces, snapshot.Workspaces.Select(MapWorkspace));
        ReplaceWith(Windows, snapshot.Windows.Select(MapWindow));
        ApplyFocus(snapshot.Focus);
        ApplyOverview(snapshot.Overview);
        ApplyDiagnostics(snapshot.Diagnostics);
        ApplyConfig(snapshot.Config);
        RefreshCounts();
    }

    public void ApplyOutputs(IEnumerable<OutputProjection> outputs, ulong nextStateVersion)
    {
        StateVersion = nextStateVersion;
        ReplaceWith(Outputs, outputs.Select(MapOutput));
        RefreshCounts();
    }

    public void ApplyWorkspaces(IEnumerable<WorkspaceProjection> workspaces, ulong nextStateVersion)
    {
        StateVersion = nextStateVersion;
        ReplaceWith(Workspaces, workspaces.Select(MapWorkspace));
        RefreshCounts();
    }

    public void ApplyWindows(IEnumerable<WindowProjection> windows, ulong nextStateVersion)
    {
        StateVersion = nextStateVersion;
        ReplaceWith(Windows, windows.Select(MapWindow));
        RefreshCounts();
    }

    public void ApplyFocus(FocusProjection focus)
    {
        FocusSummary =
            $"Window {FormatOptionalId(focus.WindowId)}, workspace {FormatOptionalId(focus.WorkspaceId)}, monitor {FormatOptionalId(focus.MonitorId)} via {NormalizeLabel(focus.Origin)}.";
    }

    public void ApplyOverview(OverviewProjection overview)
    {
        OverviewOpen = overview.IsOpen;
        OverviewSummary =
            overview.IsOpen
                ? $"Overview open on monitor {FormatOptionalId(overview.MonitorId)} with workspace selection {FormatOptionalId(overview.SelectionWorkspaceId)}."
                : "Overview is closed.";
    }

    public void ApplyDiagnostics(DiagnosticsProjection diagnostics)
    {
        var degradedFlags =
            diagnostics.DegradedFlags.Count == 0
                ? "no degraded flags"
                : string.Join(", ", diagnostics.DegradedFlags.Select(NormalizeLabel));
        var transitionLabel = string.IsNullOrWhiteSpace(diagnostics.LastTransitionLabel)
            ? "none"
            : diagnostics.LastTransitionLabel;
        DiagnosticsSummary =
            $"Records {diagnostics.TotalRecords}, last transition {transitionLabel}, management {(diagnostics.ManagementEnabled ? "enabled" : "disabled")}, degraded {degradedFlags}.";
    }

    public void ApplyConfig(ConfigProjection config)
    {
        ConfigSummary =
            $"Config v{config.ConfigVersion} from {config.SourcePath}, rules {config.ActiveRuleCount}, strip step {config.StripScrollStep}, default column mode {NormalizeLabel(config.DefaultColumnMode)}.";
    }

    private void RefreshCounts()
    {
        OutputSummary = Outputs.Count == 0
            ? "No outputs projected."
            : $"{Outputs.Count} output projection(s) available.";
        WorkspaceSummary = Workspaces.Count == 0
            ? "No workspaces projected."
            : $"{Workspaces.Count} workspace projection(s) available.";
        WindowSummary = Windows.Count == 0
            ? "No managed windows projected."
            : $"{Windows.Count} window projection(s) available.";
    }

    private static OutputCard MapOutput(OutputProjection output)
    {
        var title = output.Binding is { Length: > 0 }
            ? output.Binding
            : $"Monitor {output.MonitorId}";
        var badge = output.IsPrimary ? "Primary output" : "Secondary output";
        var geometry =
            $"{output.WorkArea.Width}x{output.WorkArea.Height} at {output.WorkArea.X},{output.WorkArea.Y}";
        var detail =
            $"{badge}, DPI {output.Dpi}, work area {geometry}";
        var workspaceLine =
            $"Workspaces {output.WorkspaceCount}, active workspace {FormatOptionalId(output.ActiveWorkspaceId)}";
        return new OutputCard(title, detail, workspaceLine);
    }

    private static WorkspaceCard MapWorkspace(WorkspaceProjection workspace)
    {
        var title = string.IsNullOrWhiteSpace(workspace.Name)
            ? $"Workspace {workspace.WorkspaceId}"
            : workspace.Name;
        var flags = new List<string>();
        if (workspace.IsActive)
        {
            flags.Add("active");
        }

        if (workspace.IsTail)
        {
            flags.Add("tail");
        }

        if (workspace.IsEmpty)
        {
            flags.Add("empty");
        }

        var status = flags.Count == 0 ? "stable" : string.Join(", ", flags);
        var detail =
            $"Monitor {workspace.MonitorId}, vertical index {workspace.VerticalIndex}, scroll {workspace.ScrollOffset}, columns {workspace.ColumnCount}";
        var occupancy =
            $"Tiled {workspace.TiledWindowCount}, floating {workspace.FloatingWindowCount}, state {status}";
        return new WorkspaceCard(title, detail, occupancy);
    }

    private static WindowCard MapWindow(WindowProjection window)
    {
        var title = string.IsNullOrWhiteSpace(window.Title)
            ? $"Window {window.WindowId}"
            : window.Title;
        var badge = window.IsFocused ? "Focused window" : "Projected window";
        var location =
            $"Workspace {window.WorkspaceId}, monitor {window.MonitorId}, column {FormatOptionalId(window.ColumnId)}";
        var detail =
            $"{badge}, {NormalizeLabel(window.Layer)}, {NormalizeLabel(window.Classification)}, class {window.ClassName}";
        var process = string.IsNullOrWhiteSpace(window.ProcessName)
            ? "process unknown"
            : $"process {window.ProcessName}";
        var management = window.IsManaged ? "managed" : "observed-only";
        return new WindowCard(title, location, $"{detail}, {process}, {management}");
    }

    private static string FormatOptionalId(ulong? value)
    {
        return value is null ? "none" : value.Value.ToString();
    }

    private static string NormalizeLabel(string value)
    {
        return value.Replace('-', ' ');
    }

    private static void ReplaceWith<T>(ObservableCollection<T> collection, IEnumerable<T> items)
    {
        collection.Clear();
        foreach (var item in items)
        {
            collection.Add(item);
        }
    }

    private bool SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return false;
        }

        field = value;
        OnPropertyChanged(propertyName);
        return true;
    }

    private void OnPropertyChanged([CallerMemberName] string? propertyName = null)
    {
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));
    }
}

public sealed record OutputCard(string Title, string Detail, string WorkspaceLine);

public sealed record WorkspaceCard(string Title, string Detail, string Occupancy);

public sealed record WindowCard(string Title, string Location, string Detail);
