# Limitations of API 0.2

The API is still changing. This is what is worth keeping in mind now.

## Security

A program sees the user's home folder, its own folder and whatever the user chose for it, see [fs](04-fs.md#what-a-program-may-access). Inside the home folder one user's programs are not separated from each other: any of them can read and change all of the user's files except `.config`. Programs have no separate data folders yet. The disk is not encrypted: on another computer it can be read without a password.

## Text

- The screen shows Latin and Cyrillic. Other scripts show as boxes.
- The keyboard types Latin and Cyrillic (English plus the Russian, Ukrainian and Belarusian layouts, enabled in Settings → Keyboard). The user chooses the language, the program does not affect it.
- Lua counts string length in bytes. For UTF-8 use `utf8.len`.

## What is not there yet

| Missing | Workaround |
|---|---|
| Reading part of a file | Only whole files |
| The file modification date | None |
| HTTPS, POST, headers | Only `http://` and GET |
| TCP and UDP sockets | None |
| Enter reported by `ui.input` | A button next to the field |
| Selection and cursor in `ui.textarea` | None |
| Closing the window from the program | None |
| A window close event | Save data right away |
| The clipboard from a program | Only through the text fields |
| Window size control | Only `app.width` and `app.height` at start |

## Compatibility

Every program runs in its own ring 3 process, but it has no child processes or threads. Do not rely on undocumented functions: everything starting with `_` (for example, `sys._dialog`) is internal and may change.
