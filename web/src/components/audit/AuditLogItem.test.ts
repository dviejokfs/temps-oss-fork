import { describe, expect, test } from 'bun:test'

import { describePermissionDenial } from '@/lib/permission-denial-display'
import { categorize } from './AuditLogItem'

describe('permission-denial audit presentation', () => {
  test('categorizes permission denials as authentication events', () => {
    expect(categorize('PERMISSION_DENIED')).toBe('auth')
  })

  test('renders only normalized, redacted denial metadata', () => {
    expect(
      describePermissionDenial({
        method: 'DELETE',
        route: '/projects/{project_id}',
        auth_source: 'api_key',
        attempt_count: 3,
      })
    ).toBe('Denied DELETE /projects/{project_id} for api key (3 attempts)')
  })

  test('handles missing optional denial metadata', () => {
    expect(describePermissionDenial()).toBe('Denied a request')
  })
})
