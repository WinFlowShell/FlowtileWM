Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class FlowtileHotkeyNative
{
    [StructLayout(LayoutKind.Sequential)]
    public struct POINT
    {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct MSG
    {
        public IntPtr hwnd;
        public uint message;
        public UIntPtr wParam;
        public IntPtr lParam;
        public uint time;
        public POINT pt;
        public uint lPrivate;
    }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool UnregisterHotKey(IntPtr hWnd, int id);

    [DllImport("user32.dll")]
    public static extern sbyte GetMessage(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax);
}
"@

$MOD_ALT = [uint32]0x0001
$MOD_CONTROL = [uint32]0x0002
$MOD_SHIFT = [uint32]0x0004
$MOD_WIN = [uint32]0x0008
$MOD_NOREPEAT = [uint32]0x4000
$WM_HOTKEY = [uint32]0x0312

function Publish-HotkeyEvent {
    param(
        [hashtable]$Payload
    )

    $json = $Payload | ConvertTo-Json -Depth 6 -Compress
    [Console]::Out.WriteLine($json)
    [Console]::Out.Flush()
}

function Resolve-VirtualKey {
    param(
        [string]$Token
    )

    $normalized = $Token.Trim().ToUpperInvariant()
    switch -Regex ($normalized) {
        '^[A-Z]$' { return [uint32][byte][char]$normalized }
        '^[0-9]$' { return [uint32][byte][char]$normalized }
        '^SPACE$' { return [uint32]0x20 }
        '^TAB$' { return [uint32]0x09 }
        '^ENTER$' { return [uint32]0x0D }
        '^ESC$|^ESCAPE$' { return [uint32]0x1B }
        '^BACKSPACE$' { return [uint32]0x08 }
        '^DELETE$|^DEL$' { return [uint32]0x2E }
        '^HOME$' { return [uint32]0x24 }
        '^END$' { return [uint32]0x23 }
        '^PAGEUP$|^PGUP$' { return [uint32]0x21 }
        '^PAGEDOWN$|^PGDN$' { return [uint32]0x22 }
        '^LEFT$' { return [uint32]0x25 }
        '^UP$' { return [uint32]0x26 }
        '^RIGHT$' { return [uint32]0x27 }
        '^DOWN$' { return [uint32]0x28 }
        '^F([1-9]|1[0-9]|2[0-4])$' {
            return [uint32](0x70 + ([int]$Matches[1] - 1))
        }
        default {
            throw "unsupported hotkey key token '$Token'"
        }
    }
}

function Parse-HotkeyTrigger {
    param(
        [string]$Trigger
    )

    $tokens = $Trigger -split '\+' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    if ($tokens.Count -eq 0) {
        throw 'empty hotkey trigger'
    }

    $modifiers = [uint32]0
    $keyToken = $null

    foreach ($rawToken in $tokens) {
        $token = $rawToken.Trim().ToLowerInvariant()
        $isModifier = $true
        switch ($token) {
            'alt' {
                $modifiers = $modifiers -bor $MOD_ALT
            }
            'ctrl' {
                $modifiers = $modifiers -bor $MOD_CONTROL
            }
            'control' {
                $modifiers = $modifiers -bor $MOD_CONTROL
            }
            'shift' {
                $modifiers = $modifiers -bor $MOD_SHIFT
            }
            'win' {
                $modifiers = $modifiers -bor $MOD_WIN
            }
            'windows' {
                $modifiers = $modifiers -bor $MOD_WIN
            }
            default {
                $isModifier = $false
            }
        }

        if ($isModifier) {
            continue
        }

        if ($null -ne $keyToken) {
            throw "hotkey trigger '$Trigger' contains more than one non-modifier token"
        }

        $keyToken = $rawToken.Trim()
    }

    if ($null -eq $keyToken) {
        throw "hotkey trigger '$Trigger' does not contain a key"
    }

    return @{
        modifiers = ($modifiers -bor $MOD_NOREPEAT)
        key = Resolve-VirtualKey -Token $keyToken
    }
}

$requestJson = [Console]::In.ReadToEnd()
if ([string]::IsNullOrWhiteSpace($requestJson)) {
    throw 'hotkey listener expected registration payload on stdin'
}

$request = $requestJson | ConvertFrom-Json -Depth 8
$registrations = @{}
$registeredIds = [System.Collections.Generic.List[int]]::new()
$nextHotkeyId = 1

try {
    foreach ($entry in $request.hotkeys) {
        $trigger = [string]$entry.trigger
        $command = [string]$entry.command
        $hotkeyId = $nextHotkeyId
        $nextHotkeyId += 1

        try {
            $parsed = Parse-HotkeyTrigger -Trigger $trigger
        }
        catch {
            Publish-HotkeyEvent @{
                kind = 'warning'
                trigger = $trigger
                command = $command
                message = $_.Exception.Message
            }
            continue
        }

        if (-not [FlowtileHotkeyNative]::RegisterHotKey(
            [IntPtr]::Zero,
            $hotkeyId,
            [uint32]$parsed.modifiers,
            [uint32]$parsed.key
        )) {
            $win32Error = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
            Publish-HotkeyEvent @{
                kind = 'warning'
                trigger = $trigger
                command = $command
                message = "RegisterHotKey failed with Win32 error $win32Error"
            }
            continue
        }

        $registrations[$hotkeyId] = @{
            trigger = $trigger
            command = $command
        }
        [void]$registeredIds.Add($hotkeyId)
    }

    if ($registeredIds.Count -eq 0) {
        Publish-HotkeyEvent @{
            kind = 'warning'
            trigger = '*'
            command = '*'
            message = 'no hotkeys were registered'
        }
        return
    }

    $message = New-Object FlowtileHotkeyNative+MSG
    while ([FlowtileHotkeyNative]::GetMessage([ref]$message, [IntPtr]::Zero, 0, 0) -gt 0) {
        if ($message.message -ne $WM_HOTKEY) {
            continue
        }

        $hotkeyId = [int]$message.wParam.ToUInt64()
        if (-not $registrations.ContainsKey($hotkeyId)) {
            continue
        }

        $registration = $registrations[$hotkeyId]
        Publish-HotkeyEvent @{
            kind = 'command'
            trigger = $registration.trigger
            command = $registration.command
        }
    }
}
finally {
    foreach ($hotkeyId in $registeredIds) {
        [void][FlowtileHotkeyNative]::UnregisterHotKey([IntPtr]::Zero, $hotkeyId)
    }
}
