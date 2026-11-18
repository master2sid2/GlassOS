# Screenshots

GlassOS in a QEMU virtual machine at 1600×900, a fresh install with one account. Everything on
the screen is drawn by GlassOS itself: the windows, the fonts, the blur behind the glass and the
wallpapers, which are generated, not image files. Some screenshots show features that land in the
next release.

## The desktop

The program menu (the gear) groups programs by category. Files and the image viewer
sit on the Mountains wallpaper, and the frosted glass of each window blurs whatever lies behind it.

![The desktop with Files, the image viewer, a clock and the program menu](images/screenshots/desktop.png)

## A Lua application and its source

Programs are Lua files: no compiler, no build step. On the left is the Widgets Demo, on the right
its own source in Notepad. Each `ui.*` call draws one widget and returns its new value. See the
[Lua API](lua-api/00-getting-started.md).

![The Widgets Demo next to its Lua source in Notepad](images/screenshots/lua-app.png)

## Task Manager and the terminal

Task Manager shows CPU, memory and the network (sent above the centre line, received below),
with every process and kernel thread. The terminal is the built-in shell; `cpu` lists the
processor's features and the load of each core.

![Task Manager next to the terminal with uname and cpu](images/screenshots/taskmgr.png)

## Settings

Every subsystem has its own page. Wallpapers come in six types, each in five colour variants.

![Settings, the Wallpaper page](images/screenshots/settings.png)

Native drop-down lists, here the screensaver type:

![Settings, the Screensaver page with the type list open](images/screenshots/settings-saver.png)

## A screensaver

Aurora, one of seven screensavers; the others include a starfield, Game of Life and a slideshow
of a picture folder.

![The Aurora screensaver](images/screenshots/saver-aurora.png)

## Sign-in

![The sign-in screen](images/screenshots/login.png)
