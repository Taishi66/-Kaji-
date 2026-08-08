# Kajiy

Put `kajiy` in your $PATH if you want to launch via:

```
kajiy .
```

This will open kaji GUI from any path you specify

# Unregister Deeplink Protocols (macos only)

`unregister-deeplink-protocols.js` is a script to unregister the deeplink protocol used by kaji like `kaji://`.
This is handy when you want to test deeplinks with the development version of Kaji.

# Usage

To unregister the deeplink protocols, run the following command in your terminal:
Then launch Kaji again and your deeplinks should work from the latest launched kaji application as it is registered on startup.

```bash
node scripts/unregister-deeplink-protocols.js
```

