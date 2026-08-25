import { describe, expect, test } from 'bun:test'
import { hostOf } from './index.js'

// hostOf is the comparison primitive that decides whether a credential
// bound to one URL may be reused for another (see ensureMcpAuth /
// findContextByUrl). It must be strict: two URLs are "the same target" only
// when their host:port match exactly, never a substring or prefix match --
// otherwise a mistyped or malicious URL could be treated as matching a
// saved, trusted context and inherit its credential.
describe('hostOf', () => {
  test('extracts host:port from a URL', () => {
    expect(hostOf('http://localhost:3000/mcp')).toBe('localhost:3000')
  })

  test('extracts bare host when no port is given (default port implied)', () => {
    expect(hostOf('https://temps.example.com/mcp')).toBe('temps.example.com')
  })

  test('is insensitive to path and query string', () => {
    expect(hostOf('https://temps.example.com/mcp?groups=platform&write=1')).toBe('temps.example.com')
    expect(hostOf('https://temps.example.com')).toBe('temps.example.com')
  })

  test('different ports on the same hostname do not match', () => {
    expect(hostOf('http://localhost:3000')).not.toBe(hostOf('http://localhost:8080'))
  })

  test('a subdomain does not match its parent domain', () => {
    expect(hostOf('https://evil.temps.example.com')).not.toBe(hostOf('https://temps.example.com'))
  })

  test('a lookalike host is not treated as the same as the real one', () => {
    expect(hostOf('https://temps.example.com.evil.net')).not.toBe(hostOf('https://temps.example.com'))
  })

  test('returns null for an unparsable URL rather than throwing', () => {
    expect(hostOf('not a url')).toBeNull()
  })
})
