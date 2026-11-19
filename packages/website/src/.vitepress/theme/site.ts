// Who builds graphpg and what has been written about it: the footer, the home page and the
// structured data in config.mts read these, so a link changes in one place.
export const author = {
  name: "Mihai Farcas",
  alternateName: "Mihai Farcaș",
  site: "https://mihai.ltd",
  github: "https://github.com/mihailtd",
  githubHandle: "mihailtd",
  linkedin: "https://www.linkedin.com/in/mihai-farcas-ltd/",
  youtube: "https://youtube.com/@letstalkdev",
  devto: "https://dev.to/mihailtd",
  company: {
    name: "innovi.pro",
    displayName: "Innovi Pro",
    url: "https://innovi.pro",
    email: "contact@innovi.pro",
    description: "AI automation, agents and software architecture consultancy",
  },
};

export const ecosystem = [
  {
    id: "https://innovi.care/#software",
    name: "Innovi Care",
    label: "innovi.care",
    url: "https://innovi.care",
    category: "HealthApplication, MedicalApplication",
    description: "Multilingual clinical practice platform and patient portal built on PostgreSQL RLS and graphpg.",
  },
  {
    id: "https://retroagile.com/#software",
    name: "RetroAgile",
    label: "retroagile.com",
    url: "https://retroagile.com",
    category: "DeveloperApplication",
    description: "Continuous retrospectives, agent churn auditing and spec-readiness scoring powered by graphpg.",
  },
];

export const articles = [
  {
    title: 'How end-to-end type safety turns "change it everywhere" into a list your AI agent can finish',
    description:
      "Why SQL, a generated contract and a type checker let an agent change something everywhere without missing a place.",
    url: "https://mihai.ltd/blog/end-to-end-type-safety-ai-coding-agents",
  },
  {
    title: "Why GraphQL and REST should share one engine: what I learned building graphpg",
    description:
      "What PostGraphile, Hasura and PostgREST taught me, what graphpg does differently, and why GraphQL and REST should share one engine.",
    url: "https://mihai.ltd/blog/building-graphpg-graphql-and-rest-from-postgres",
  },
  {
    title: "Why Row-Level Security Is the Only Reliable Auth for AI-Built Apps",
    description:
      "Why AI coding agents routinely introduce data leaks in application-layer authorization, and how PostgreSQL RLS turns authorization into an auditable data-layer barrier.",
    url: "https://mihai.ltd/blog/row-level-security-ai-coding-agents",
  },
];
