import { describe, expect, test } from 'bun:test'

import { PERMISSION_DENIED_FILTER } from '@/lib/audit-operation-filters'

describe('audit operation filters', () => {
  test('offers permission denials in the authentication group', () => {
    expect(PERMISSION_DENIED_FILTER).toEqual({
      value: 'PERMISSION_DENIED',
      label: 'Permission Denied',
    })
  })
})
