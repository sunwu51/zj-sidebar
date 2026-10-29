# Optional tweaks

None of this is needed for the plugin. It's the setup the plugin was developed with.

## zellij: theme and shell

Zellij draws its own UI and does not inherit your terminal's theme. In `config.kdl`:

```kdl
theme "catppuccin-mocha"          // any built-in or custom theme
default_shell "pwsh"              // Windows: full path, e.g. "C:/Program Files/PowerShell/7/pwsh.exe"
```

## zellij: keys that collide with programs inside it

Zellij binds several `Ctrl` keys by default. Two that hurt when editing in a terminal editor (micro, nano…):

- `Ctrl+S` → scroll mode, so the editor never sees "save";
- `Ctrl+Q` → quit zellij.

Keep `Ctrl+Q` for quitting zellij if you like, and move scroll mode to `Alt+S`: in `config.kdl`, inside `keybinds`, replace `bind "Ctrl s" { SwitchToMode "scroll"; }` by `bind "Alt s" { SwitchToMode "scroll"; }` (and the same in the `scroll`/`search` mode block that leaves scroll mode). Zellij's `locked` mode (`Ctrl+g`) passes everything through.

## Edit the current command line in an editor (`Ctrl+X Ctrl+E`)

**bash**: built in (`$EDITOR`, or set `export EDITOR=micro`).

**zsh**:

```zsh
autoload -z edit-command-line; zle -N edit-command-line
bindkey '^x^e' edit-command-line
```

**PowerShell** (`$PROFILE`):

```powershell
$env:EDITOR = 'micro'     # or 'code --wait'
Set-PSReadLineKeyHandler -Chord 'Ctrl+x,Ctrl+e' -Function ViEditVisually
```

The edited text comes back to the prompt when the editor exits (PowerShell does not run it automatically, so you can review it).

Mouse works in [micro](https://micro-editor.github.io) (`scoop install micro` / `brew install micro`), including click-to-place and drag-select. To make `Ctrl+S` save **and quit** in micro only for this use, give it its own config dir:

```sh
mkdir -p ~/.config/micro-cmdline
echo '{ "Ctrl-s": "Save&Quit" }' > ~/.config/micro-cmdline/bindings.json
# wrapper script `micro-cmdline` on your PATH:
#!/bin/sh
MICRO_CONFIG_HOME="$HOME/.config/micro-cmdline" exec micro "$@"
```

then `export EDITOR=micro-cmdline`. (Windows: a `.cmd` wrapper setting `MICRO_CONFIG_HOME=%USERPROFILE%\.config\micro-cmdline`.)

## PowerShell: multi-line input

`\` is not a line continuation in PowerShell (the backtick `` ` `` is). Leave a bracket/quote open, or press `Shift+Enter` for a new line. Windows Terminal doesn't send Shift+Enter by default; to make it work, add this action and binding to `settings.json`:

```json
"actions": [{ "command": { "action": "sendInput", "input": "\n" }, "id": "User.ShiftEnterNewline" }],
"keybindings": [{ "id": "User.ShiftEnterNewline", "keys": "shift+enter" }]
```

and in `$PROFILE` (the `\n` arrives as `Ctrl+Enter`, which PSReadLine maps to "insert line above"):

```powershell
Set-PSReadLineKeyHandler -Chord 'Ctrl+Enter' -Function AddLine
```

## PowerShell with posh-git: short prompt path

Turns `C:\Users\me\Desktop\code\project` into `~\D\c\project`:

```powershell
$GitPromptSettings.DefaultPromptAbbreviateHomeDirectory = $true
function global:Get-ShortPromptPath {
    $parts = (Get-PromptPath) -split '[\\/]'
    if ($parts.Count -le 2) { return $parts -join '\' }
    $head = for ($i = 0; $i -lt $parts.Count - 1; $i++) {
        $s = $parts[$i]
        if ($s -eq '~' -or $s -like '*:' -or $s.Length -le 1) { $s }
        elseif ($s.StartsWith('.')) { $s.Substring(0, 2) } else { $s.Substring(0, 1) }
    }
    (@($head) + $parts[-1]) -join '\'
}
$GitPromptSettings.DefaultPromptPath.Text = '$(Get-ShortPromptPath)'
```
