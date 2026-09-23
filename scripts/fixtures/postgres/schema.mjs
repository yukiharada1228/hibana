import { integer, pgTable, text } from "drizzle-orm/pg-core";

export const entries = pgTable("hibana_extension_probe", {
  id: integer("id").primaryKey(),
  label: text("label").notNull(),
});
