import "vitest";

declare module "vitest" {
  export interface ProvidedContext {
    graphqlUrl: string;
  }
}
