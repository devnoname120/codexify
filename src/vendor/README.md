# Embedded Markdown parser

`markdown-it.min.js` is the browser distribution from the npm package
`markdown-it@14.3.1`, under the MIT license in `markdown-it.LICENSE`. The widget
embeds both the parser and its license; it never loads a script from a CDN.

To refresh, use `npm pack markdown-it@<version> --ignore-scripts` and copy
`package/dist/markdown-it.min.js` and `package/LICENSE`. Run the Markdown chat
browser tests and the Rust resource test after updating. The renderer consumes
parser tokens into safe DOM nodes with raw HTML disabled, rather than inserting
the parser's HTML output.
