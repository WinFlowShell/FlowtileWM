namespace Flowtile.TouchpadHelper;

internal enum TouchpadGesture
{
    ThreeFingerSwipeLeft,
    ThreeFingerSwipeRight,
    ThreeFingerSwipeUp,
    ThreeFingerSwipeDown,
    FourFingerSwipeLeft,
    FourFingerSwipeRight,
    FourFingerSwipeUp,
    FourFingerSwipeDown,
}

internal sealed class GestureRecognizer
{
    private SwipeSession? _session;

    internal TouchpadGesture? ProcessContacts(IReadOnlyList<TouchpadContact> contacts)
    {
        var fingerCount = contacts.Count;
        if (fingerCount is < 3 or > 4)
        {
            return FinishCurrentSession();
        }

        var centroid = Centroid(contacts);
        _session ??= new SwipeSession(fingerCount, centroid, centroid);
        _session = _session with
        {
            FingerCount = Math.Max(_session.FingerCount, fingerCount),
            LastCentroid = centroid,
        };

        return null;
    }

    private TouchpadGesture? FinishCurrentSession()
    {
        var session = _session;
        _session = null;
        if (session is null)
        {
            return null;
        }

        const int SwipeThreshold = 120;
        const int DominanceNumerator = 3;
        const int DominanceDenominator = 2;

        var deltaX = session.LastCentroid.X - session.StartCentroid.X;
        var deltaY = session.LastCentroid.Y - session.StartCentroid.Y;
        var absX = Math.Abs(deltaX);
        var absY = Math.Abs(deltaY);
        if (absX < SwipeThreshold && absY < SwipeThreshold)
        {
            return null;
        }

        var horizontal = absX * DominanceDenominator >= absY * DominanceNumerator;
        var vertical = absY * DominanceDenominator >= absX * DominanceNumerator;

        return (session.FingerCount, horizontal, vertical, Math.Sign(deltaX), Math.Sign(deltaY)) switch
        {
            (3, true, false, > 0, _) => TouchpadGesture.ThreeFingerSwipeRight,
            (3, true, false, < 0, _) => TouchpadGesture.ThreeFingerSwipeLeft,
            (3, false, true, _, > 0) => TouchpadGesture.ThreeFingerSwipeDown,
            (3, false, true, _, < 0) => TouchpadGesture.ThreeFingerSwipeUp,
            (4, true, false, > 0, _) => TouchpadGesture.FourFingerSwipeRight,
            (4, true, false, < 0, _) => TouchpadGesture.FourFingerSwipeLeft,
            (4, false, true, _, > 0) => TouchpadGesture.FourFingerSwipeDown,
            (4, false, true, _, < 0) => TouchpadGesture.FourFingerSwipeUp,
            _ => null,
        };
    }

    private static Point Centroid(IReadOnlyList<TouchpadContact> contacts)
    {
        var sumX = 0L;
        var sumY = 0L;
        foreach (var contact in contacts)
        {
            sumX += contact.X;
            sumY += contact.Y;
        }

        var count = Math.Max(contacts.Count, 1);
        return new Point((int)(sumX / count), (int)(sumY / count));
    }

    private readonly record struct Point(int X, int Y);
    private sealed record SwipeSession(int FingerCount, Point StartCentroid, Point LastCentroid);
}
