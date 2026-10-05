export interface Options {
  /** Registry URL; default https://registry.npmjs.org */
  registry?: string;
  /** Path of an .npmrc to apply. Nothing is discovered; a `${VAR}` reference is an error. */
  npmrc?: string;
  /** Contents of an .npmrc the host already holds, applied after `npmrc`. */
  npmrcContents?: string;
  /** Parallel fetches; default 16. */
  concurrency?: number;
  /** Registry per scope, `{ "@acme": "https://npm.acme.dev/" }`: what an `@acme:registry` key does. */
  scopedRegistries?: Record<string, string>;
  /** `authorization` header value per URL prefix, `{ "https://npm.acme.dev/": "Bearer tok" }`: what a `//npm.acme.dev/:_authToken` key does. The longest matching prefix wins. */
  auth?: Record<string, string>;
}

export interface Root {
  name: string;
  version: string;
  /** `<dir>/node_modules/<name>` */
  dir: string;
}

export interface Installation {
  /** The requested packages: in request order for a list, in name order for an object. */
  roots: Root[];
  /** Command → absolute script path; the same commands are linked in node_modules/.bin. */
  bins: Record<string, string>;
  /** Tarballs extracted by this call. */
  packages: number;
  /** `name@version` of every package whose install script was not run. */
  skippedInstallScripts: string[];
}

/** Install `["eslint@^9", "prettier"]` or `{ eslint: "^9" }` into `<dir>/node_modules`. */
export function install(deps: string[] | Record<string, string>, dir: string, options?: Options): Promise<Installation>;
export function installSync(deps: string[] | Record<string, string>, dir: string, options?: Options): Installation;
