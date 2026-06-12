# Nexus Marketing Assets

Promotional visuals for Nexus. Use these in tweets, landing pages, slide decks, or anywhere you want a sharp visual of what Nexus delivers.

## Files

| File | Format | Best for |
|------|--------|----------|
| `nexus-gain-showcase.html` | HTML | Embed in a landing page, screenshot in browser |
| `nexus-gain-showcase.svg` | SVG | Twitter cards, slide decks, sharp PNG export |

## How to use

### Get a clean screenshot from the HTML

```bash
open marketing/nexus-gain-showcase.html
```

Opens in your default browser. Take a screenshot of the terminal window (Cmd+Shift+4 on macOS, then space to capture the window).

### Convert SVG to PNG (for socials / pinned tweet)

```bash
# macOS — using qlmanage (built-in)
qlmanage -t -s 1840 -o marketing marketing/nexus-gain-showcase.svg

# Or with rsvg-convert (brew install librsvg)
rsvg-convert -w 1840 marketing/nexus-gain-showcase.svg > marketing/nexus-gain-showcase.png

# Or with Inkscape
inkscape --export-type=png --export-width=1840 marketing/nexus-gain-showcase.svg
```

The SVG is designed at 920×620 logical units — exports beautifully at 2× (1840×1240) for retina screens.

### Embed in your README

```markdown
![Nexus savings](marketing/nexus-gain-showcase.svg)
```

## About the numbers

These mockups depict **a typical week of AI-assisted development** (~1,247 commands) showing **96.3% average savings**. Numbers are based on real Nexus filter performance benchmarks but represent an aggregated week of use, not any specific user's data.

Clearly labeled as `Sample output` in the footer of each visual, so anyone reading knows they're seeing a showcase, not a personal screenshot.

## License

Same as the main project — Apache 2.0. Use them freely.
