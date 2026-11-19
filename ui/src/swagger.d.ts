declare module 'swagger-ui-dist/swagger-ui-es-bundle.js' {
  interface SwaggerRequest {
    url: string;
    method?: string;
    headers: Record<string, string>;
    [k: string]: unknown;
  }
  interface SwaggerOptions {
    domNode: HTMLElement;
    url?: string;
    spec?: unknown;
    deepLinking?: boolean;
    tryItOutEnabled?: boolean;
    validatorUrl?: string | null;
    persistAuthorization?: boolean;
    displayRequestDuration?: boolean;
    filter?: boolean;
    docExpansion?: 'list' | 'full' | 'none';
    defaultModelsExpandDepth?: number;
    syntaxHighlight?: { activated?: boolean; theme?: string } | false;
    requestInterceptor?: (req: SwaggerRequest) => SwaggerRequest | Promise<SwaggerRequest>;
    [k: string]: unknown;
  }
  interface SwaggerInstance {
    [k: string]: unknown;
  }
  const SwaggerUI: (opts: SwaggerOptions) => SwaggerInstance;
  export default SwaggerUI;
}
