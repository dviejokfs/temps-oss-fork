import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'

import { ProjectListPagination } from './ProjectListPagination'

describe('ProjectListPagination', () => {
  test('renders page-size, direct-page, and boundary navigation controls', () => {
    const markup = renderToStaticMarkup(
      <ProjectListPagination
        page={3}
        pageSize={18}
        total={100}
        totalPages={6}
        onPageChange={() => {}}
        onPageSizeChange={() => {}}
      />
    )

    expect(markup).toContain(
      '<span class="hidden sm:inline">Showing 37–54 of 100 projects</span>'
    )
    expect(markup).toContain('<span class="sm:hidden">3 / 6</span>')
    expect(markup).toContain('aria-label="Projects per page"')
    expect(markup).toContain('aria-label="Page number"')
    expect(markup).toContain('aria-label="Go to first page"')
    expect(markup).toContain('aria-label="Go to last page"')
    expect(markup).toContain('>Go</button>')
  })
})
