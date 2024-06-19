# where-am-i

This is a tool for displaying your VRChat location while livestreaming.

It works by reading the VRChat logs and exposing a simple web interface for browser sources.

## Overlays

To display a simple information bar across the bottom of the screen, launch where-am-i.exe and then add a browser source to OBS using the URL "http://127.0.0.1:37544". Adjust the width and height in the browser source properties.

The program also comes with some individual overlay elements:
- http://127.0.0.1:37544/world.html displays the image of the current world.
- http://127.0.0.1:37544/qr.html displays a QR code for the world page.
- http://127.0.0.1:37544/join.html displays a QR code that allows viewers to join the instance _maybe even if it is private_ ⚠️

### Customization

The files in the static directory may be edited to change the appearance of the overlays. This can be done while the program is running.

It's possible to create an overlay that detects when VRChat is loading (location changes to null) while OBS is displaying the a VRChat scene and trigger a transition to a loading scene, then transition back when VRChat finishes loading. https://github.com/obsproject/obs-browser?tab=readme-ov-file#control-obs

## Bots

If you are running a chat bot from your computer, you can use these URLs to implement chat commands:
- http://127.0.0.1:37544/api/world/current/info.txt returns information about the current world
- http://127.0.0.1:37544/api/room/current/link.txt returns a link that allows viewers to join the instance _maybe even if it is private_ ⚠️

## API

http://127.0.0.1:37544/api/status is a server-sent event stream that sends "location" events with a JSON object describing the current location, or null if not currently in a world.

/api/world/:worldId/image gets the VRChat world image. VRChat's world image URL is part of the location information, but this API takes care of caching the image locally.

/api/world/:worldId/qr.svg gets a QR code for the world link.

/api/room/:roomId/qr.svg gets a QR code for an instance join link. ⚠️
