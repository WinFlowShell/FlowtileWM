namespace Flowtile.TouchpadHelper;

internal readonly record struct TouchpadContact(int ContactId, int X, int Y)
{
    public override string ToString() => $"id={ContactId} x={X} y={Y}";
}

internal sealed class TouchpadContactBuilder
{
    internal int? ContactId { get; set; }
    internal int? X { get; set; }
    internal int? Y { get; set; }

    internal bool TryBuild(out TouchpadContact contact)
    {
        if (ContactId.HasValue && X.HasValue && Y.HasValue)
        {
            contact = new TouchpadContact(ContactId.Value, X.Value, Y.Value);
            return true;
        }

        contact = default;
        return false;
    }

    internal void Clear()
    {
        ContactId = null;
        X = null;
        Y = null;
    }
}
