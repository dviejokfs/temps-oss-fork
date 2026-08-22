import { describe, expect, test } from 'bun:test'
import { buildMcpUrl, isValidGroupKey, TOOL_GROUPS } from './groups.js'

describe('isValidGroupKey', () => {
  test('accepts every declared group key', () => {
    for (const group of TOOL_GROUPS) {
      expect(isValidGroupKey(group.key)).toBe(true)
    }
  })

  test('rejects an unknown key', () => {
    expect(isValidGroupKey('not-a-group')).toBe(false)
  })
})

describe('buildMcpUrl', () => {
  test('omits groups and write when all groups selected and write disabled', () => {
    const url = buildMcpUrl('http://localhost:3000', TOOL_GROUPS.map((g) => g.key), false)
    expect(url).toBe('http://localhost:3000/mcp')
  })

  test('includes groups when a subset is selected', () => {
    const url = buildMcpUrl('http://localhost:3000', ['deployments', 'observability'], false)
    expect(url).toBe('http://localhost:3000/mcp?groups=deployments%2Cobservability')
  })

  test('includes write=1 when write is enabled', () => {
    const url = buildMcpUrl('http://localhost:3000', TOOL_GROUPS.map((g) => g.key), true)
    expect(url).toBe('http://localhost:3000/mcp?write=1')
  })

  test('strips a trailing slash from the configured API URL', () => {
    const url = buildMcpUrl('http://localhost:3000/', TOOL_GROUPS.map((g) => g.key), false)
    expect(url).toBe('http://localhost:3000/mcp')
  })

  test('combines groups and write', () => {
    const url = buildMcpUrl('https://temps.example.com', ['platform'], true)
    expect(url).toBe('https://temps.example.com/mcp?groups=platform&write=1')
  })
})
