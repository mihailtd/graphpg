<script setup lang="ts">
// The home page's type-safety picture: a migration flowing through graphpg into generated types,
// and the editor before and after a column rename. The timings are the ones measured in
// docs/type-safety.md (FHIR R5 schema, 177 tables); keep the two in step.
import { withBase } from "vitepress";
import { ref } from "vue";

const renamed = ref(false);

const stages = [
  { label: "Migration", detail: "SQL or Drizzle", file: "0014_rename.sql" },
  { label: "PostgreSQL", detail: "the catalog is the source", file: "pg_catalog" },
  {
    label: "graphpg",
    detail: "dev mode rewrites the contract",
    file: "schema.graphql · openapi.json",
    time: "0.6–1.0 s",
  },
  { label: "Codegen", detail: "GraphQL Code Generator, openapi-typescript", file: "src/gql · api.d.ts", time: "5–6 s" },
  { label: "Type checker", detail: "editor and CI see every break", file: "tsc", time: "0.3 s" },
];
</script>

<template>
  <div class="typeflow">
    <ol class="typeflow-chain" aria-label="From a migration to type errors in the app">
      <li v-for="(stage, i) in stages" :key="stage.label" class="typeflow-stage" :class="{ hot: renamed && i === stages.length - 1 }">
        <span class="typeflow-index">{{ i + 1 }}</span>
        <strong>{{ stage.label }}</strong>
        <span class="typeflow-detail">{{ stage.detail }}</span>
        <code class="typeflow-file">{{ stage.file }}</code>
        <span v-if="stage.time" class="typeflow-time">{{ stage.time }}</span>
      </li>
    </ol>

    <div class="typeflow-demo">
      <div class="typeflow-toolbar">
        <span class="typeflow-title">
          {{ renamed ? "After the migration: three errors in src/patients.ts" : "Before: the app compiles" }}
        </span>
        <button type="button" class="typeflow-toggle" :aria-pressed="renamed" @click="renamed = !renamed">
          {{ renamed ? "Undo the rename" : "Rename family → family_name" }}
        </button>
      </div>

      <div class="typeflow-panes">
        <figure class="typeflow-pane">
          <figcaption>src/schema.ts <span>Drizzle</span></figcaption>
          <pre><code><span class="tk">export const</span> patients = pgTable(<span class="st">"patients"</span>, {
  id: uuid(<span class="st">"id"</span>).primaryKey(),
<template v-if="!renamed">  family: text(<span class="st">"family"</span>),
</template><template v-else><span class="del">- family: text("family"),</span>
<span class="add">+ familyName: text("family_name"),</span>
</template>  given: text(<span class="st">"given"</span>),
});</code></pre>
        </figure>

        <figure class="typeflow-pane">
          <figcaption>src/patients.ts <span>GraphQL + REST</span></figcaption>
          <pre><code>graphql(<span class="st">`query PatientList {
  patientsConnection(orderBy: [<span :class="{ squiggle: renamed }">FAMILY_ASC</span>]) {
    nodes { id <span :class="{ squiggle: renamed }">family</span> given }
  } }`</span>);

api.GET(<span class="st">"/rest/patients"</span>, { params: {
  query: { fields: [<span class="st">"id"</span>, <span class="st" :class="{ squiggle: renamed }">"family"</span>] } } });</code></pre>
        </figure>
      </div>

      <div class="typeflow-problems" role="status" aria-live="polite">
        <template v-if="renamed">
          <p><span class="err">✖</span> Value "FAMILY_ASC" does not exist in "PatientOrderBy" enum. Did you mean "FAMILY_NAME_ASC"?</p>
          <p><span class="err">✖</span> Cannot query field "family" on type "Patient".</p>
          <p><span class="err">✖</span> TS2322: Type '"family"' is not assignable to type '"id" | "familyName" | "given" | …'.</p>
        </template>
        <p v-else><span class="ok">✓</span> No problems. Every field, filter and column name is checked against the database.</p>
      </div>
    </div>

    <p class="typeflow-note">
      Measured on a 177-table FHIR R5 schema with graphpg 1.1.0.
      <a :href="withBase('/docs/type-safety#what-a-rename-looks-like')">How it was measured →</a>
    </p>
  </div>
</template>

<style scoped>
.typeflow {
  margin-top: 8px;
}

.typeflow-chain {
  list-style: none;
  margin: 0;
  padding: 0;
  display: grid;
  gap: 12px;
  grid-template-columns: 1fr;
  counter-reset: none;
}

@media (min-width: 640px) {
  .typeflow-chain {
    grid-template-columns: repeat(2, 1fr);
  }
}

@media (min-width: 960px) {
  .typeflow-chain {
    grid-template-columns: repeat(5, 1fr);
  }
}

.typeflow-stage {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 16px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--vp-c-bg-soft);
  transition:
    border-color 0.25s,
    box-shadow 0.25s;
}

/* arrows between the stages on wide screens */
@media (min-width: 960px) {
  .typeflow-stage:not(:last-child)::after {
    content: "";
    position: absolute;
    top: 50%;
    right: -11px;
    width: 10px;
    height: 10px;
    border-top: 2px solid var(--vp-c-brand-1);
    border-right: 2px solid var(--vp-c-brand-1);
    transform: translateY(-50%) rotate(45deg);
    z-index: 1;
  }
}

.typeflow-stage.hot {
  border-color: var(--vp-c-danger-1);
  box-shadow: 0 0 0 3px var(--vp-c-danger-soft);
}

.typeflow-index {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border-radius: 50%;
  font-size: 12px;
  font-weight: 600;
  color: var(--vp-c-white);
  background: linear-gradient(135deg, #3a7cbd, #b8459f);
}

.typeflow-stage strong {
  font-size: 15px;
}

.typeflow-detail {
  font-size: 13px;
  line-height: 1.4;
  color: var(--vp-c-text-2);
}

.typeflow-file {
  align-self: flex-start;
  margin-top: 4px;
  font-size: 12px !important;
}

.typeflow-time {
  align-self: flex-start;
  margin-top: 4px;
  padding: 1px 8px;
  border-radius: 999px;
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  font-weight: 600;
  color: var(--vp-c-text-1);
  background: var(--vp-c-brand-soft);
}

.typeflow-demo {
  margin-top: 20px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  overflow: hidden;
  background: var(--vp-code-block-bg);
}

.typeflow-toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 16px;
  border-bottom: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-soft);
}

.typeflow-title {
  font-size: 14px;
  font-weight: 600;
}

.typeflow-toggle {
  min-height: 36px;
  padding: 0 16px;
  border-radius: 18px;
  font-size: 14px;
  font-weight: 600;
  color: var(--vp-button-brand-text);
  background: var(--vp-button-brand-bg);
  transition: background-color 0.25s;
}

.typeflow-toggle:hover {
  background: var(--vp-button-brand-hover-bg);
}

.typeflow-toggle:focus-visible {
  outline: 2px solid var(--vp-c-brand-1);
  outline-offset: 2px;
}

.typeflow-panes {
  display: grid;
  grid-template-columns: 1fr;
}

@media (min-width: 768px) {
  .typeflow-panes {
    grid-template-columns: 1fr 1fr;
  }
  .typeflow-pane + .typeflow-pane {
    border-left: 1px solid var(--vp-c-divider);
  }
}

.typeflow-pane {
  margin: 0;
  min-width: 0;
}

.typeflow-pane figcaption {
  display: flex;
  justify-content: space-between;
  padding: 10px 16px 0;
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  color: var(--vp-c-text-2);
}

.typeflow-pane pre {
  margin: 0;
  padding: 12px 16px 16px;
  overflow-x: auto;
  font-size: 13px;
  line-height: 1.6;
}

.typeflow-pane code {
  font-family: var(--vp-font-family-mono);
  color: var(--vp-c-text-1);
  background: none;
  padding: 0;
}

.tk {
  color: var(--vp-c-brand-1);
}

.st {
  color: var(--vp-c-green-1, #18794e);
}

.del {
  color: var(--vp-c-danger-1);
}

.add {
  color: var(--vp-c-green-1, #18794e);
}

.squiggle {
  text-decoration: underline wavy var(--vp-c-danger-1);
  text-decoration-skip-ink: none;
  text-underline-offset: 4px;
}

.typeflow-problems {
  padding: 12px 16px;
  border-top: 1px solid var(--vp-c-divider);
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  line-height: 1.6;
  background: var(--vp-c-bg-soft);
}

.typeflow-problems p {
  margin: 0;
  color: var(--vp-c-text-2);
}

.err {
  color: var(--vp-c-danger-1);
  margin-right: 6px;
}

.ok {
  color: var(--vp-c-green-1, #18794e);
  margin-right: 6px;
}

.typeflow-note {
  margin-top: 12px;
  font-size: 13px;
  color: var(--vp-c-text-2);
}

@media (prefers-reduced-motion: reduce) {
  .typeflow-stage,
  .typeflow-toggle {
    transition: none;
  }
}
</style>
