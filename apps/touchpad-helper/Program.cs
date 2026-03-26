namespace Flowtile.TouchpadHelper;

internal static class Program
{
    [STAThread]
    private static void Main()
    {
        Logger.Info("flowtile-touchpad-helper starting");
        ApplicationConfiguration.Initialize();
        using var window = new HiddenTouchpadWindow();
        _ = window.Handle;
        Application.Run(window);
    }
}
