import { Alert, Anchor, Group, Stack, Text } from '@mantine/core';
import { IconExternalLink } from '@tabler/icons-react';
import { useEffect, useRef, useState } from 'react';
import 'swagger-ui-dist/swagger-ui.css';
import { tokenStore } from '../api/http';
import { sameOrigin } from '../lib/origin';
import { PageHeader } from '../components/common';
import { LoadingState } from '../components/States';
import classes from './ApiDocs.module.css';

export default function ApiDocs() {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<'loading' | 'ready' | 'error'>('loading');

  useEffect(() => {
    let cancelled = false;
    void import('swagger-ui-dist/swagger-ui-es-bundle.js')
      .then((m) => {
        if (cancelled || !host.current) return;
        m.default({
          domNode: host.current,
          url: '/openapi.json',
          deepLinking: false,
          tryItOutEnabled: true,
          validatorUrl: null,
          persistAuthorization: false,
          displayRequestDuration: true,
          filter: true,
          docExpansion: 'list',
          defaultModelsExpandDepth: 0,
          requestInterceptor: (req) => {
            const token = tokenStore.get();
            if (token && sameOrigin(req.url, window.location.href) && !req.headers.Authorization) req.headers.Authorization = `Bearer ${token}`;
            return req;
          },
        });
        setState('ready');
      })
      .catch(() => {
        if (!cancelled) setState('error');
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <Stack gap="md">
      <PageHeader
        title="API docs"
        description={
          <Group gap={6}>
            <Text size="sm" c="dimmed">
              The admin API, described by OpenAPI. “Try it out” sends requests with your current token.
            </Text>
            <Anchor href="/openapi.json" target="_blank" rel="noopener noreferrer" size="sm">
              <Group gap={2}>
                openapi.json <IconExternalLink size={14} />
              </Group>
            </Anchor>
          </Group>
        }
      />
      {state === 'loading' && <LoadingState label="Loading Swagger UI…" />}
      {state === 'error' && <Alert color="red">Swagger UI could not be loaded.</Alert>}
      <div ref={host} className={classes.host} data-testid="swagger-host" />
    </Stack>
  );
}
