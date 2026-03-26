using System.Runtime.InteropServices;

namespace Flowtile.TouchpadHelper;

internal static class TouchpadRawInput
{
    internal const int WmInput = 0x00FF;

    internal static bool TouchpadExists()
    {
        uint deviceCount = 0;
        var listEntrySize = (uint)Marshal.SizeOf<RawInputDeviceListEntry>();
        if (GetRawInputDeviceList(null!, ref deviceCount, listEntrySize) != 0)
        {
            return false;
        }

        var devices = new RawInputDeviceListEntry[deviceCount];
        if (GetRawInputDeviceList(devices, ref deviceCount, listEntrySize) != deviceCount)
        {
            return false;
        }

        return devices
            .Where(device => device.Type == RimTypeHid)
            .Any(device => IsPrecisionTouchpad(device.DeviceHandle));
    }

    internal static bool RegisterInput(IntPtr windowHandle)
    {
        var devices = new[]
        {
            new RawInputDevice
            {
                UsagePage = 0x000D,
                Usage = 0x0005,
                Flags = RidevInputSink | RidevDevNotify,
                WindowHandle = windowHandle,
            },
        };

        return RegisterRawInputDevices(devices, (uint)devices.Length, (uint)Marshal.SizeOf<RawInputDevice>());
    }

    internal static IReadOnlyList<TouchpadContact> ParseInput(IntPtr lParam)
    {
        uint rawInputSize = 0;
        var headerSize = (uint)Marshal.SizeOf<RawInputHeader>();
        if (GetRawInputData(lParam, RidInput, IntPtr.Zero, ref rawInputSize, headerSize) != 0)
        {
            return Array.Empty<TouchpadContact>();
        }

        var rawInputPointer = IntPtr.Zero;
        var rawHidPointer = IntPtr.Zero;
        var preparsedPointer = IntPtr.Zero;

        try
        {
            rawInputPointer = Marshal.AllocHGlobal((int)rawInputSize);
            if (GetRawInputData(lParam, RidInput, rawInputPointer, ref rawInputSize, headerSize) != rawInputSize)
            {
                return Array.Empty<TouchpadContact>();
            }

            var rawInput = Marshal.PtrToStructure<RawInput>(rawInputPointer);
            if (rawInput.Header.Type != RimTypeHid)
            {
                return Array.Empty<TouchpadContact>();
            }

            var rawInputData = new byte[rawInputSize];
            Marshal.Copy(rawInputPointer, rawInputData, 0, rawInputData.Length);

            var rawHidBytes = new byte[rawInput.Hid.Size * rawInput.Hid.Count];
            var hidOffset = rawInputData.Length - rawHidBytes.Length;
            Buffer.BlockCopy(rawInputData, hidOffset, rawHidBytes, 0, rawHidBytes.Length);

            rawHidPointer = Marshal.AllocHGlobal(rawHidBytes.Length);
            Marshal.Copy(rawHidBytes, 0, rawHidPointer, rawHidBytes.Length);

            uint preparsedSize = 0;
            if (GetRawInputDeviceInfo(rawInput.Header.DeviceHandle, RidiPreparsedData, IntPtr.Zero, ref preparsedSize) != 0)
            {
                return Array.Empty<TouchpadContact>();
            }

            preparsedPointer = Marshal.AllocHGlobal((int)preparsedSize);
            if (GetRawInputDeviceInfo(rawInput.Header.DeviceHandle, RidiPreparsedData, preparsedPointer, ref preparsedSize) != preparsedSize)
            {
                return Array.Empty<TouchpadContact>();
            }

            if (HidP_GetCaps(preparsedPointer, out var caps) != HidpStatusSuccess)
            {
                return Array.Empty<TouchpadContact>();
            }

            var valueCapsLength = caps.NumberInputValueCaps;
            var valueCaps = new HidpValueCaps[valueCapsLength];
            if (HidP_GetValueCaps(HidpReportType.Input, valueCaps, ref valueCapsLength, preparsedPointer) != HidpStatusSuccess)
            {
                return Array.Empty<TouchpadContact>();
            }

            var contacts = new List<TouchpadContact>();
            var creators = new List<TouchpadContactBuilder>();
            uint contactCount = 99;

            foreach (var valueCap in valueCaps.OrderBy(cap => cap.LinkCollection))
            {
                for (var contactIndex = 0; contactIndex < rawInput.Hid.Count; contactIndex++)
                {
                    var reportPointer = IntPtr.Add(rawHidPointer, (int)(rawInput.Hid.Size * contactIndex));
                    if (HidP_GetUsageValue(
                            HidpReportType.Input,
                            valueCap.UsagePage,
                            valueCap.LinkCollection,
                            valueCap.Usage,
                            out var value,
                            preparsedPointer,
                            reportPointer,
                            (uint)rawHidBytes.Length) != HidpStatusSuccess)
                    {
                        continue;
                    }

                    switch (valueCap.LinkCollection)
                    {
                        case 0:
                            if (valueCap is { UsagePage: 0x0D, Usage: 0x54 })
                            {
                                contactCount = value;
                            }
                            break;
                        default:
                            while (creators.Count <= contactIndex)
                            {
                                creators.Add(new TouchpadContactBuilder());
                            }

                            switch (valueCap.UsagePage, valueCap.Usage)
                            {
                                case (0x0D, 0x51):
                                    creators[contactIndex].ContactId = (int)value;
                                    break;
                                case (0x01, 0x30):
                                    creators[contactIndex].X = (int)value;
                                    break;
                                case (0x01, 0x31):
                                    creators[contactIndex].Y = (int)value;
                                    break;
                            }
                            break;
                    }
                }

                foreach (var creator in creators)
                {
                    if ((contactCount == 0 || contacts.Count < contactCount) && creator.TryBuild(out var contact))
                    {
                        contacts.Add(contact);
                        creator.Clear();
                    }
                }

                if (contactCount != 0 && contacts.Count >= contactCount)
                {
                    break;
                }
            }

            return contacts;
        }
        finally
        {
            if (preparsedPointer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(preparsedPointer);
            }

            if (rawHidPointer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(rawHidPointer);
            }

            if (rawInputPointer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(rawInputPointer);
            }
        }
    }

    private static bool IsPrecisionTouchpad(IntPtr deviceHandle)
    {
        uint deviceInfoSize = 0;
        if (GetRawInputDeviceInfo(deviceHandle, RidiDeviceInfo, IntPtr.Zero, ref deviceInfoSize) != 0)
        {
            return false;
        }

        var deviceInfo = new RidDeviceInfo { Size = deviceInfoSize };
        if (GetRawInputDeviceInfo(deviceHandle, RidiDeviceInfo, ref deviceInfo, ref deviceInfoSize) == unchecked((uint)-1))
        {
            return false;
        }

        return deviceInfo.Hid.UsagePage == 0x000D && deviceInfo.Hid.Usage == 0x0005;
    }

    private const uint RimTypeHid = 2;
    private const uint RidevInputSink = 0x00000100;
    private const uint RidevDevNotify = 0x00002000;
    private const uint RidInput = 0x10000003;
    private const uint RidiPreparsedData = 0x20000005;
    private const uint RidiDeviceInfo = 0x2000000b;
    private const uint HidpStatusSuccess = 0x00110000;

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint GetRawInputDeviceList(
        [Out] RawInputDeviceListEntry[] pRawInputDeviceList,
        ref uint puiNumDevices,
        uint cbSize);

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool RegisterRawInputDevices(
        RawInputDevice[] pRawInputDevices,
        uint uiNumDevices,
        uint cbSize);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint GetRawInputData(
        IntPtr hRawInput,
        uint uiCommand,
        IntPtr pData,
        ref uint pcbSize,
        uint cbSizeHeader);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint GetRawInputDeviceInfo(
        IntPtr hDevice,
        uint uiCommand,
        IntPtr pData,
        ref uint pcbSize);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint GetRawInputDeviceInfo(
        IntPtr hDevice,
        uint uiCommand,
        ref RidDeviceInfo pData,
        ref uint pcbSize);

    [DllImport("hid.dll", SetLastError = true)]
    private static extern uint HidP_GetCaps(IntPtr preparsedData, out HidpCaps capabilities);

    [DllImport("hid.dll", CharSet = CharSet.Auto)]
    private static extern uint HidP_GetValueCaps(
        HidpReportType reportType,
        [Out] HidpValueCaps[] valueCaps,
        ref ushort valueCapsLength,
        IntPtr preparsedData);

    [DllImport("hid.dll", CharSet = CharSet.Auto)]
    private static extern uint HidP_GetUsageValue(
        HidpReportType reportType,
        ushort usagePage,
        ushort linkCollection,
        ushort usage,
        out uint usageValue,
        IntPtr preparsedData,
        IntPtr report,
        uint reportLength);

    [StructLayout(LayoutKind.Sequential)]
    private struct RawInputDeviceListEntry
    {
        internal IntPtr DeviceHandle;
        internal uint Type;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RawInputDevice
    {
        internal ushort UsagePage;
        internal ushort Usage;
        internal uint Flags;
        internal IntPtr WindowHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RawInput
    {
        internal RawInputHeader Header;
        internal RawHid Hid;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RawInputHeader
    {
        internal uint Type;
        internal uint Size;
        internal IntPtr DeviceHandle;
        internal IntPtr WParam;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RawHid
    {
        internal uint Size;
        internal uint Count;
        internal IntPtr RawData;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RidDeviceInfo
    {
        internal uint Size;
        internal uint Type;
        internal RidDeviceInfoHid Hid;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct RidDeviceInfoHid
    {
        internal uint VendorId;
        internal uint ProductId;
        internal uint VersionNumber;
        internal ushort UsagePage;
        internal ushort Usage;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct HidpCaps
    {
        internal ushort Usage;
        internal ushort UsagePage;
        internal ushort InputReportByteLength;
        internal ushort OutputReportByteLength;
        internal ushort FeatureReportByteLength;

        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 17)]
        internal ushort[] Reserved;

        internal ushort NumberLinkCollectionNodes;
        internal ushort NumberInputButtonCaps;
        internal ushort NumberInputValueCaps;
        internal ushort NumberInputDataIndices;
        internal ushort NumberOutputButtonCaps;
        internal ushort NumberOutputValueCaps;
        internal ushort NumberOutputDataIndices;
        internal ushort NumberFeatureButtonCaps;
        internal ushort NumberFeatureValueCaps;
        internal ushort NumberFeatureDataIndices;
    }

    private enum HidpReportType
    {
        Input,
        Output,
        Feature,
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct HidpValueCaps
    {
        internal ushort UsagePage;
        internal byte ReportId;

        [MarshalAs(UnmanagedType.U1)]
        internal bool IsAlias;

        internal ushort BitField;
        internal ushort LinkCollection;
        internal ushort LinkUsage;
        internal ushort LinkUsagePage;

        [MarshalAs(UnmanagedType.U1)]
        internal bool IsRange;

        [MarshalAs(UnmanagedType.U1)]
        internal bool IsStringRange;

        [MarshalAs(UnmanagedType.U1)]
        internal bool IsDesignatorRange;

        [MarshalAs(UnmanagedType.U1)]
        internal bool IsAbsolute;

        [MarshalAs(UnmanagedType.U1)]
        internal bool HasNull;

        internal byte Reserved;
        internal ushort BitSize;
        internal ushort ReportCount;

        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 5)]
        internal ushort[] Reserved2;

        internal uint UnitsExp;
        internal uint Units;
        internal int LogicalMin;
        internal int LogicalMax;
        internal int PhysicalMin;
        internal int PhysicalMax;
        internal ushort UsageMin;
        internal ushort UsageMax;
        internal ushort StringMin;
        internal ushort StringMax;
        internal ushort DesignatorMin;
        internal ushort DesignatorMax;
        internal ushort DataIndexMin;
        internal ushort DataIndexMax;

        internal ushort Usage => UsageMin;
    }
}
