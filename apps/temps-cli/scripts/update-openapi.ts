#!/usr/bin/env bun
/**
 * Refresh `openapi.json` from a running temps server, in a canonical shape.
 *
 * Why this exists: the server emits the spec minified and with whatever key
 * order serde produced. Writing that straight to disk replaces a ~92,000-line
 * pretty-printed file with a single line, so the pull request reports ~-92,000
 * deletions and the real change is invisible to reviewers. Key order is not
 * stable across builds either, so even a pretty-printed dump reorders large
 * blocks for no reason.
 *
 * Canonical form is therefore: keys sorted, two-space indent, trailing
 * newline. Sorting is what makes the diff proportional to the API change
 * rather than to serde's iteration order.
 *
 * Usage:
 *   bun run scripts/update-openapi.ts                     # localhost:8080
 *   bun run scripts/update-openapi.ts --url http://localhost:8220/api/api-docs/openapi.json
 *   TEMPS_API_KEY=tk_... bun run scripts/update-openapi.ts # if the server requires auth
 *
 * Then regenerate the client from the file:
 *   bun run generate:api
 */

const DEFAULT_URL = 'http://localhost:8080/api/api-docs/openapi.json'
const OUTPUT = new URL('../openapi.json', import.meta.url).pathname

/** Recursively sort object keys so the on-disk order never depends on the server. */
function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) {
    // Array order is meaningful in OpenAPI (parameter lists, enum values) —
    // sort the contents, never the sequence.
    return value.map(canonicalize)
  }
  if (value && typeof value === 'object') {
    const source = value as Record<string, unknown>
    return Object.fromEntries(
      Object.keys(source)
        .sort()
        .map((key) => [key, canonicalize(source[key])])
    )
  }
  return value
}

function parseArgs(argv: string[]): { url: string } {
  const index = argv.indexOf('--url')
  if (index !== -1) {
    const url = argv[index + 1]
    if (!url) {
      console.error('--url requires a value')
      process.exit(1)
    }
    return { url }
  }
  return { url: process.env.TEMPS_OPENAPI_URL ?? DEFAULT_URL }
}

const { url } = parseArgs(process.argv.slice(2))
const apiKey = process.env.TEMPS_API_KEY

const response = await fetch(url, {
  headers: apiKey ? { Authorization: `Bearer ${apiKey}` } : {},
}).catch((error: unknown) => {
  console.error(`Could not reach ${url}: ${String(error)}`)
  console.error('Start a temps server first, or pass --url.')
  process.exit(1)
})

if (!response.ok) {
  console.error(`${url} returned ${response.status}`)
  if (response.status === 401 || response.status === 403) {
    console.error('Set TEMPS_API_KEY to an admin key; the spec endpoint is authenticated.')
  }
  process.exit(1)
}

const spec = await response.json()
if (!spec?.paths || Object.keys(spec.paths).length === 0) {
  // A spec with no paths means the server answered but the doc was not
  // assembled — writing it would silently delete the entire committed client.
  console.error('Refusing to write: the fetched spec has no paths.')
  process.exit(1)
}

await Bun.write(OUTPUT, `${JSON.stringify(canonicalize(spec), null, 2)}\n`)
console.log(`Wrote ${OUTPUT} (${Object.keys(spec.paths).length} paths)`)
console.log('Now run: bun run generate:api')
