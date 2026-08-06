import { defineConfig } from '@hey-api/openapi-ts'

export default defineConfig({
  input: 'openapi.json',
  // input: 'http://localhost:3000/api-docs/openapi.json',
  output: {
    path: 'src/api',
  },
  plugins: ['@hey-api/sdk', '@hey-api/typescript'],
})
