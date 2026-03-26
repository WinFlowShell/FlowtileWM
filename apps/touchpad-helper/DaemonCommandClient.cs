using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Flowtile.TouchpadHelper;

internal sealed class DaemonCommandClient
{
    private const string CommandPipeName = "flowtilewm-ipc-v1";
    private const int ProtocolVersion = 1;

    internal bool TrySendGesture(TouchpadGesture gesture, out string detail)
    {
        var request = new IpcRequest(
            ProtocolVersion,
            $"touchpad-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()}-{Guid.NewGuid():N}",
            "touchpad_gesture",
            new Dictionary<string, string>
            {
                ["gesture"] = gesture.ToWireName(),
            });

        try
        {
            using var pipe = new NamedPipeClientStream(
                ".",
                CommandPipeName,
                PipeDirection.InOut,
                PipeOptions.None);
            pipe.Connect(3000);
            pipe.ReadMode = PipeTransmissionMode.Message;

            var requestJson = JsonSerializer.Serialize(request);
            var requestBytes = Encoding.UTF8.GetBytes(requestJson);
            pipe.Write(requestBytes, 0, requestBytes.Length);
            pipe.Flush();

            using var reader = new MemoryStream();
            var buffer = new byte[64 * 1024];
            do
            {
                var read = pipe.Read(buffer, 0, buffer.Length);
                if (read <= 0)
                {
                    break;
                }

                reader.Write(buffer, 0, read);
            } while (!pipe.IsMessageComplete);

            var responseJson = Encoding.UTF8.GetString(reader.ToArray());
            var response = JsonSerializer.Deserialize<IpcResponse>(responseJson);
            if (response is null)
            {
                detail = "daemon returned an empty IPC response";
                return false;
            }

            if (!response.Ok)
            {
                detail = response.Error?.Message ?? "daemon rejected the touchpad gesture";
                return false;
            }

            detail = response.Result.HasValue ? response.Result.Value.GetRawText() : "ok";
            return true;
        }
        catch (Exception error)
        {
            detail = error.Message;
            return false;
        }
    }

    private sealed record IpcRequest(
        [property: JsonPropertyName("protocol_version")] int ProtocolVersion,
        [property: JsonPropertyName("request_id")] string RequestId,
        [property: JsonPropertyName("command")] string Command,
        [property: JsonPropertyName("payload")] Dictionary<string, string> Payload);

    private sealed record IpcResponse(
        [property: JsonPropertyName("protocol_version")] int ProtocolVersion,
        [property: JsonPropertyName("request_id")] string RequestId,
        [property: JsonPropertyName("ok")] bool Ok,
        [property: JsonPropertyName("result")] JsonElement? Result,
        [property: JsonPropertyName("error")] IpcError? Error);

    private sealed record IpcError(
        [property: JsonPropertyName("code")] string Code,
        [property: JsonPropertyName("message")] string Message,
        [property: JsonPropertyName("category")] string Category,
        [property: JsonPropertyName("retryable")] bool Retryable);
}
