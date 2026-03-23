using Microsoft.UI.Xaml;

namespace Flowtile.UiHost;

public sealed partial class MainWindow : Window
{
    private readonly FlowtileDaemonClient daemonClient;
    private DiagnosticsWindow? diagnosticsWindow;

    public MainWindow()
    {
        State = new FlowtileShellState();
        InitializeComponent();
        daemonClient = new FlowtileDaemonClient(DispatcherQueue, State);
        daemonClient.Start();
        Closed += MainWindow_Closed;
    }

    public FlowtileShellState State { get; }

    private async void FocusPrevButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("focus_prev");
    }

    private async void FocusNextButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("focus_next");
    }

    private async void ScrollLeftButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("scroll_strip_left");
    }

    private async void ScrollRightButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("scroll_strip_right");
    }

    private async void ToggleOverviewButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("toggle_overview");
    }

    private async void ReloadConfigButton_Click(object sender, RoutedEventArgs e)
    {
        await daemonClient.SendCommandAsync("reload_config");
    }

    private void OpenDiagnosticsButton_Click(object sender, RoutedEventArgs e)
    {
        if (diagnosticsWindow is null)
        {
            diagnosticsWindow = new DiagnosticsWindow(State);
            diagnosticsWindow.Closed += DiagnosticsWindow_Closed;
        }

        diagnosticsWindow.Activate();
    }

    private void DiagnosticsWindow_Closed(object sender, WindowEventArgs args)
    {
        if (sender is DiagnosticsWindow window)
        {
            window.Closed -= DiagnosticsWindow_Closed;
        }

        diagnosticsWindow = null;
    }

    private async void MainWindow_Closed(object sender, WindowEventArgs args)
    {
        Closed -= MainWindow_Closed;
        if (diagnosticsWindow is not null)
        {
            diagnosticsWindow.Close();
            diagnosticsWindow = null;
        }

        await daemonClient.DisposeAsync();
    }
}
