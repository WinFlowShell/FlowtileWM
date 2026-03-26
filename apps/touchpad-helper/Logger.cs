using System.Text;

namespace Flowtile.TouchpadHelper;

internal static class Logger
{
    private static readonly object Gate = new();
    private static readonly string? FilePath = Environment.GetEnvironmentVariable("FLOWTILE_TOUCHPAD_HELPER_LOG_PATH");

    internal static void Info(string message)
    {
        WriteLine(message);
    }

    private static void WriteLine(string message)
    {
        var line = $"[{DateTimeOffset.UtcNow:O}] {message}";
        Console.WriteLine(line);

        if (string.IsNullOrWhiteSpace(FilePath))
        {
            return;
        }

        lock (Gate)
        {
            var directory = Path.GetDirectoryName(FilePath);
            if (!string.IsNullOrWhiteSpace(directory))
            {
                Directory.CreateDirectory(directory);
            }

            File.AppendAllText(FilePath, line + Environment.NewLine, Encoding.UTF8);
        }
    }
}
