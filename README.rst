######################
 Bangle.js 2 emulator
######################

The online Bangle.js emulator, accessible through the `Espruino IDE`_, is pretty
neat and useful for app development, but it doesn't help with firmware
development—and some people might prefer not to have to use a browser as a
development environment in any case. This repository contains a standalone
emulator that can be used for developing Bangle.js 2 apps or firmware without a
physical device. (Even if you have a physical device, using the emulator will
likely be faster and more convenient, unless of course you are developing
something that relies on the real hardware.)

Features:

-  TUI showing screen and console output
-  touchscreen and button input
-  device console served over TCP
-  config file for conveniently specifying initial emulator state

Current non-features:

-  reset/interrupt on button hold
-  sensor inputs
-  screen lock/backlight tracking

************************
 Installation and usage
************************

Clone this repository and set up a recent Rust_ toolchain.

You'll need a version of the Espruino_ firmware compiled to WebAssembly_. The
Espruino upstream_ currently does not support creating the necessary WebAssembly
build; see the ``wasm`` `branch of my fork`_ to get a version with the relevant
changes. You can download a precompiled version from the `GitHub Actions`_ there
or clone the repository and perform the build yourself (install Emscripten_ and
run ``RELEASE=1 BOARD=EMSCRIPTEN2 make``; the output will be in
``bin/emulator_banglejs2.wasm``).

You can also use a TOML_ config file to specify the state of the emulated watch
on startup (by default, the watch will start with nothing in storage, like in
the Espruino IDE). The file ``sample-config.toml`` in this repository
demonstrates a basic config and some commented examples.

Once you have a WebAssembly firmware file and optionally a config file at hand,
run

.. code:: sh

   cargo run --release [-c <config file>] <firmware file>

to start the emulator. The screen and console output will be displayed in the
terminal; you can click on the screen to provide touch inputs, including drags
and swipes, and press Enter to press the button. The emulator also exposes the
emulated watch's console over TCP (listening on ``localhost:37026`` by default;
use ``-b`` to change). Running ``rlwrap nc localhost 37026`` or ``socat readline
tcp:localhost:37026`` (see rlwrap_, netcat_, socat_) will connect to the console
with a somewhat shell-like experience.

Press q or Escape to quit.

*****************
 Installing apps
*****************

You can have apps (including widgets and clocks) installed on startup by using
the config file to place the required files onto the watch's storage. The sample
config file contains some examples of how to do this.

The most important file is the JavaScript code itself. The standard, as defined
by the `App Loader`_, is for this to be called ``<appname>.app.js`` (for apps
and clocks) or ``<appname>.wid.js`` (for widgets), though Espruino itself
doesn't really care. This is the same thing that would normally be placed on the
watch by the App Loader, apart from minification, so you can specify the
contents by providing a path to a file in a BangleApps_ clone or wherever else
you might be working on your code. You can start the app by using the
``load(<file>)`` function in Espruino.

If you're using the standard launcher (which will be present after a factory
reset), you'll also need a file called ``<appname>.info`` for the app to show up
in the launcher, containing a JSON object describing the metadata for the app.
Normally, this file is generated dynamically by the App Loader and doesn't exist
otherwise, so you'll probably want to specify the contents directly in the
config file.

Apps may also make use of other files, such as images or settings files. The
details will vary from app to app; each app's ``metadata.json`` file describes
what files it uses.

*********
 License
*********

Licensed under either of `Apache License, Version 2.0`_ or `MIT license`_ at
your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

.. _apache license, version 2.0: https://www.apache.org/licenses/LICENSE-2.0

.. _app loader: https://banglejs.com/apps/

.. _bangleapps: https://github.com/espruino/BangleApps

.. _branch of my fork: https://github.com/dzhu/Espruino/tree/wasm

.. _emscripten: https://emscripten.org

.. _espruino: https://www.espruino.com

.. _espruino ide: https://www.espruino.com/ide/

.. _github actions: https://github.com/dzhu/Espruino/actions

.. _mit license: https://opensource.org/licenses/MIT

.. _netcat: https://en.wikipedia.org/wiki/Netcat

.. _rlwrap: https://github.com/hanslub42/rlwrap

.. _rust: https://www.rust-lang.org

.. _socat: http://www.dest-unreach.org/socat/

.. _toml: https://toml.io

.. _upstream: https://github.com/espruino/Espruino

.. _webassembly: https://webassembly.org
