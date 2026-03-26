namespace Flowtile.TouchpadHelper;

internal static class TouchpadGestureExtensions
{
    internal static string ToWireName(this TouchpadGesture gesture)
    {
        return gesture switch
        {
            TouchpadGesture.ThreeFingerSwipeLeft => "three-finger-swipe-left",
            TouchpadGesture.ThreeFingerSwipeRight => "three-finger-swipe-right",
            TouchpadGesture.ThreeFingerSwipeUp => "three-finger-swipe-up",
            TouchpadGesture.ThreeFingerSwipeDown => "three-finger-swipe-down",
            TouchpadGesture.FourFingerSwipeLeft => "four-finger-swipe-left",
            TouchpadGesture.FourFingerSwipeRight => "four-finger-swipe-right",
            TouchpadGesture.FourFingerSwipeUp => "four-finger-swipe-up",
            TouchpadGesture.FourFingerSwipeDown => "four-finger-swipe-down",
            _ => throw new ArgumentOutOfRangeException(nameof(gesture), gesture, null),
        };
    }
}
