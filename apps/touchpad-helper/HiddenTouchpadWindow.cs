using System.Runtime.InteropServices;

namespace Flowtile.TouchpadHelper;

internal sealed class HiddenTouchpadWindow : Form
{
    private readonly DaemonCommandClient _daemonClient = new();
    private readonly GestureRecognizer _recognizer = new();
    private IReadOnlyList<TouchpadContact> _lastContacts = Array.Empty<TouchpadContact>();

    internal HiddenTouchpadWindow()
    {
        ShowInTaskbar = false;
        FormBorderStyle = FormBorderStyle.FixedToolWindow;
        WindowState = FormWindowState.Minimized;
        Opacity = 0;
        Width = 1;
        Height = 1;
        Text = "Flowtile Touchpad Helper";
    }

    protected override void SetVisibleCore(bool value)
    {
        base.SetVisibleCore(false);
    }

    protected override void OnHandleCreated(EventArgs e)
    {
        base.OnHandleCreated(e);

        var touchpadExists = TouchpadRawInput.TouchpadExists();
        Logger.Info($"precision-touchpad-exists={touchpadExists}");
        if (!touchpadExists)
        {
            Logger.Info("No Precision Touchpad device was detected.");
            return;
        }

        var registered = TouchpadRawInput.RegisterInput(Handle);
        Logger.Info($"touchpad-register-input={registered}");
        if (!registered)
        {
            Logger.Info($"touchpad-register-input-error={Marshal.GetLastWin32Error()}");
        }
    }

    protected override void WndProc(ref Message m)
    {
        if (m.Msg == TouchpadRawInput.WmInput)
        {
            var contacts = TouchpadRawInput.ParseInput(m.LParam);
            if (contacts.Count > 0 || _lastContacts.Count > 0)
            {
                var joined = contacts.Count == 0 ? "<none>" : string.Join(", ", contacts);
                Logger.Info($"contacts=[{joined}]");
            }

            var gesture = _recognizer.ProcessContacts(contacts);
            if (gesture.HasValue)
            {
                Logger.Info($"gesture={gesture.Value}");
                if (_daemonClient.TrySendGesture(gesture.Value, out var detail))
                {
                    Logger.Info($"daemon-dispatch-ok gesture={gesture.Value} detail={detail}");
                }
                else
                {
                    Logger.Info($"daemon-dispatch-error gesture={gesture.Value} detail={detail}");
                }
            }

            _lastContacts = contacts;
        }

        base.WndProc(ref m);
    }
}
