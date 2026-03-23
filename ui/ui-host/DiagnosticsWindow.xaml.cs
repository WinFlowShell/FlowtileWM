using Microsoft.UI.Xaml;

namespace Flowtile.UiHost;

public sealed partial class DiagnosticsWindow : Window
{
    public DiagnosticsWindow(FlowtileShellState state)
    {
        State = state;
        InitializeComponent();
    }

    public FlowtileShellState State { get; }
}
