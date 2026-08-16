import { beforeEach, describe, expect, it, vi } from 'vitest';
import { TempsClient } from './index';
import * as clientModule from './client/client';
import * as sdk from './client/sdk.gen';

vi.mock('./client/client', () => ({
  createClient: vi.fn(() => ({
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
    patch: vi.fn(),
  })),
  createConfig: vi.fn((config) => config),
}));

vi.mock('./client/sdk.gen', () => ({
  listEmailDomainProjects: vi.fn().mockResolvedValue({ data: [] }),
  authorizeEmailDomainProject: vi.fn().mockResolvedValue({ data: undefined }),
  revokeEmailDomainProject: vi.fn().mockResolvedValue({ data: undefined }),
}));

describe('TempsClient', () => {
  let client: TempsClient;

  beforeEach(() => {
    vi.clearAllMocks();
    client = new TempsClient({
      baseUrl: 'https://api.test.com',
      apiKey: 'test-api-key',
    });
  });

  it('creates one authenticated client shared by every namespace', () => {
    expect(clientModule.createConfig).toHaveBeenCalledWith({
      baseUrl: 'https://api.test.com',
      headers: { Authorization: 'Bearer test-api-key' },
    });
    expect(client.rawClient).toBeDefined();

    for (const namespace of [
      'apiKeys', 'analytics', 'auditLogs', 'authentication', 'backups',
      'crons', 'deployments', 'dns', 'domains', 'email', 'externalServices',
      'files', 'funnels', 'git', 'loadBalancer', 'monitoring', 'notifications',
      'performance', 'platform', 'projects', 'proxyLogs', 'repositories',
      'sessionReplay', 'settings', 'users',
    ]) {
      expect(client).toHaveProperty(namespace);
    }
  });

  it('omits the authorization header when no API key is configured', () => {
    vi.clearAllMocks();
    new TempsClient({ baseUrl: 'https://api.test.com' });

    expect(clientModule.createConfig).toHaveBeenCalledWith({
      baseUrl: 'https://api.test.com',
      headers: undefined,
    });
  });

  it('routes email-domain project reads and writes through the configured client', async () => {
    const listOptions = { path: { id: 7 } };
    const writeOptions = { path: { id: 7, project_id: 42 } };

    await client.email.listAuthorizedProjects(listOptions);
    await client.email.authorizeProject(writeOptions);
    await client.email.revokeProject(writeOptions);

    expect(sdk.listEmailDomainProjects).toHaveBeenCalledWith({
      ...listOptions,
      client: client.rawClient,
    });
    expect(sdk.authorizeEmailDomainProject).toHaveBeenCalledWith({
      ...writeOptions,
      client: client.rawClient,
    });
    expect(sdk.revokeEmailDomainProject).toHaveBeenCalledWith({
      ...writeOptions,
      client: client.rawClient,
    });
  });

  it('propagates authorization failures to the caller', async () => {
    vi.mocked(sdk.authorizeEmailDomainProject).mockRejectedValueOnce(new Error('forbidden'));

    await expect(
      client.email.authorizeProject({ path: { id: 7, project_id: 42 } }),
    ).rejects.toThrow('forbidden');
  });
});
