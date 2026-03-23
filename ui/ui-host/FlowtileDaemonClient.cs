using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using Microsoft.UI.Dispatching;

namespace Flowtile.UiHost;

public sealed class FlowtileDaemonClient : IAsyncDisposable
{
    private const string CommandPipeName = "flowtilewm-ipc-v1";
    private const string EventStreamPipeName = "flowtilewm-event-stream-v1";

    private static readonly TimeSpan PipeConnectTimeout = TimeSpan.FromSeconds(3);
    private static readonly TimeSpan ReconnectDelay = TimeSpan.FromSeconds(1);

    private readonly DispatcherQueue dispatcherQueue;
    private readonly FlowtileShellState shellState;
    private readonly JsonSerializerOptions jsonOptions;
    private readonly CancellationTokenSource shutdown = new();

    private Task? eventLoopTask;

    public FlowtileDaemonClient(DispatcherQueue dispatcherQueue, FlowtileShellState shellState)
    {
        this.dispatcherQueue = dispatcherQueue;
        this.shellState = shellState;
        jsonOptions = new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            PropertyNameCaseInsensitive = true,
        };
    }

    public void Start()
    {
        eventLoopTask ??= Task.Run(() => RunEventStreamLoopAsync(shutdown.Token));
    }

    public async Task SendCommandAsync(string command, object? payload = null)
    {
        try
        {
            using var pipe = CreatePipeClient(CommandPipeName);
            using var timeoutSource = CancellationTokenSource.CreateLinkedTokenSource(shutdown.Token);
            timeoutSource.CancelAfter(PipeConnectTimeout);

            await pipe.ConnectAsync(timeoutSource.Token);
            pipe.ReadMode = PipeTransmissionMode.Message;

            var request = new IpcRequestEnvelope
            {
                RequestId = $"ui-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()}",
                Command = command,
                Payload = payload ?? new { },
            };
            await WriteMessageAsync(
                pipe,
                JsonSerializer.Serialize(request, jsonOptions),
                timeoutSource.Token);

            var responseText = await ReadMessageAsync(pipe, timeoutSource.Token);
            if (string.IsNullOrWhiteSpace(responseText))
            {
                await EnqueueAsync(
                    () => shellState.MarkCommandResult(
                        $"Command {NormalizeLabel(command)} returned an empty response."));
                return;
            }

            var response = JsonSerializer.Deserialize<IpcResponseEnvelope>(responseText, jsonOptions);
            if (response is null)
            {
                await EnqueueAsync(
                    () => shellState.MarkCommandResult(
                        $"Command {NormalizeLabel(command)} returned malformed JSON."));
                return;
            }

            var label = response.Ok
                ? $"Command {NormalizeLabel(command)} accepted by daemon."
                : $"Command {NormalizeLabel(command)} failed: {response.Error?.Message ?? "unknown error"}.";
            await EnqueueAsync(() => shellState.MarkCommandResult(label));
        }
        catch (OperationCanceledException) when (shutdown.IsCancellationRequested)
        {
        }
        catch (Exception error)
        {
            await EnqueueAsync(
                () => shellState.MarkCommandResult(
                    $"Command {NormalizeLabel(command)} failed: {error.Message}."));
        }
    }

    public async ValueTask DisposeAsync()
    {
        shutdown.Cancel();
        if (eventLoopTask is not null)
        {
            try
            {
                await eventLoopTask;
            }
            catch (OperationCanceledException)
            {
            }
        }

        shutdown.Dispose();
    }

    private async Task RunEventStreamLoopAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested)
        {
            await EnqueueAsync(shellState.MarkConnecting);

            try
            {
                using var pipe = CreatePipeClient(EventStreamPipeName);
                using var timeoutSource = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
                timeoutSource.CancelAfter(PipeConnectTimeout);

                await pipe.ConnectAsync(timeoutSource.Token);
                pipe.ReadMode = PipeTransmissionMode.Message;
                await EnqueueAsync(shellState.MarkConnected);

                while (!cancellationToken.IsCancellationRequested)
                {
                    var message = await ReadMessageAsync(pipe, cancellationToken);
                    if (message is null)
                    {
                        throw new IOException("event stream ended unexpectedly");
                    }

                    var trimmed = message.Trim();
                    if (trimmed.Length == 0)
                    {
                        continue;
                    }

                    var envelope = JsonSerializer.Deserialize<IpcEventEnvelope>(trimmed, jsonOptions);
                    if (envelope is null)
                    {
                        continue;
                    }

                    await EnqueueAsync(() =>
                    {
                        shellState.RecordEvent(envelope.EventKind, envelope.StateVersion);
                        ApplyEventEnvelope(envelope);
                    });
                }
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                break;
            }
            catch (Exception error)
            {
                await EnqueueAsync(() => shellState.MarkDisconnected(error.Message));
            }

            try
            {
                await Task.Delay(ReconnectDelay, cancellationToken);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                break;
            }
        }
    }

    private void ApplyEventEnvelope(IpcEventEnvelope envelope)
    {
        switch (envelope.EventKind)
        {
            case "snapshot_begin":
                shellState.RecordEvent("snapshot begin", envelope.StateVersion);
                break;
            case "snapshot_state":
                ApplyPayload<SnapshotProjection>(
                    envelope.Payload,
                    snapshot => shellState.ApplySnapshot(snapshot));
                break;
            case "snapshot_end":
                shellState.RecordEvent("snapshot end", envelope.StateVersion);
                break;
            case "monitor_changed":
                ApplyPayload<OutputsDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyOutputs(payload.Outputs, envelope.StateVersion));
                break;
            case "workspace_changed":
                ApplyPayload<WorkspacesDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyWorkspaces(payload.Workspaces, envelope.StateVersion));
                break;
            case "window_changed":
                ApplyPayload<WindowsDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyWindows(payload.Windows, envelope.StateVersion));
                break;
            case "focus_changed":
                ApplyPayload<FocusDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyFocus(payload.Focus));
                break;
            case "overview_changed":
                ApplyPayload<OverviewDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyOverview(payload.Overview));
                break;
            case "diagnostic_notice":
                ApplyPayload<DiagnosticsDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyDiagnostics(payload.Diagnostics));
                break;
            case "config_changed":
                ApplyPayload<ConfigDelta>(
                    envelope.Payload,
                    payload => shellState.ApplyConfig(payload.Config));
                break;
            default:
                shellState.RecordEvent(envelope.EventKind, envelope.StateVersion);
                break;
        }
    }

    private void ApplyPayload<T>(JsonElement payload, Action<T> apply)
    {
        var value = payload.Deserialize<T>(jsonOptions);
        if (value is not null)
        {
            apply(value);
        }
    }

    private static NamedPipeClientStream CreatePipeClient(string pipeName)
    {
        return new NamedPipeClientStream(
            ".",
            pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous);
    }

    private async Task EnqueueAsync(Action action)
    {
        var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!dispatcherQueue.TryEnqueue(() =>
            {
                try
                {
                    action();
                    completion.SetResult();
                }
                catch (Exception error)
                {
                    completion.SetException(error);
                }
            }))
        {
            completion.SetException(
                new InvalidOperationException("UI dispatcher queue is unavailable."));
        }

        await completion.Task;
    }

    private static async Task<string?> ReadMessageAsync(
        PipeStream pipe,
        CancellationToken cancellationToken)
    {
        var buffer = new byte[4096];
        using var payload = new MemoryStream();

        while (true)
        {
            var read = await pipe.ReadAsync(buffer, cancellationToken);
            if (read == 0)
            {
                return payload.Length == 0
                    ? null
                    : Encoding.UTF8.GetString(payload.GetBuffer(), 0, (int)payload.Length);
            }

            payload.Write(buffer, 0, read);
            if (pipe is NamedPipeClientStream namedPipe && namedPipe.IsMessageComplete)
            {
                return Encoding.UTF8.GetString(payload.GetBuffer(), 0, (int)payload.Length);
            }
        }
    }

    private static async Task WriteMessageAsync(
        PipeStream pipe,
        string payload,
        CancellationToken cancellationToken)
    {
        var bytes = Encoding.UTF8.GetBytes(payload);
        await pipe.WriteAsync(bytes, cancellationToken);
        await pipe.FlushAsync(cancellationToken);
    }

    private static string NormalizeLabel(string value)
    {
        return value.Replace('_', ' ').Replace('-', ' ');
    }
}
