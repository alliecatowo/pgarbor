import { relations, sql } from "drizzle-orm";
import {
  bigint, bigserial, boolean, index, integer, jsonb, numeric, pgEnum, pgTable,
  primaryKey, serial, text, timestamp, unique, uuid, varchar,
} from "drizzle-orm/pg-core";

// --- migration 0000 baseline ---
export const userRole = pgEnum("user_role", ["admin", "editor", "viewer"]);

export const organizations = pgTable("organizations", {
  id: serial("id").primaryKey(),
  name: varchar("name", { length: 120 }).notNull().unique(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

export const users = pgTable("users", {
  id: uuid("id").primaryKey().defaultRandom(),
  orgId: integer("org_id").notNull().references(() => organizations.id, { onDelete: "cascade" }),
  email: text("email").notNull(),
  displayName: varchar("display_name", { length: 80 }).notNull(),
  role: userRole("role").notNull().default("viewer"),
  active: boolean("active").notNull().default(true),
  settings: jsonb("settings").$type<{ theme?: string; notifications?: { email: boolean } }>().notNull().default({}),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  // --- migration 0001 additions ---
  lastLoginAt: timestamp("last_login_at", { withTimezone: true }),
  karma: bigint("karma", { mode: "number" }).notNull().default(0),
}, (t) => [
  unique("users_email_unique").on(t.email),
  index("users_org_idx").on(t.orgId),
]);

export const posts = pgTable("posts", {
  id: bigint("id", { mode: "number" }).primaryKey().generatedAlwaysAsIdentity(),
  authorId: uuid("author_id").notNull().references(() => users.id, { onDelete: "cascade" }),
  title: text("title").notNull(),
  body: text("body"),
  tags: text("tags").array().notNull().default(sql`'{}'::text[]`),
  scores: integer("scores").array(),
  price: numeric("price", { precision: 10, scale: 2 }),
  published: boolean("published").notNull().default(false),
  meta: jsonb("meta"),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  // --- migration 0001 additions ---
  viewCount: integer("view_count").notNull().default(0),
}, (t) => [
  index("posts_author_created_idx").on(t.authorId, t.createdAt),
]);

export const comments = pgTable("comments", {
  id: bigserial("id", { mode: "number" }).primaryKey(),
  postId: bigint("post_id", { mode: "number" }).notNull().references(() => posts.id, { onDelete: "cascade" }),
  authorId: uuid("author_id").references(() => users.id, { onDelete: "set null" }),
  body: text("body").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

export const postLikes = pgTable("post_likes", {
  postId: bigint("post_id", { mode: "number" }).notNull().references(() => posts.id, { onDelete: "cascade" }),
  userId: uuid("user_id").notNull().references(() => users.id, { onDelete: "cascade" }),
  likedAt: timestamp("liked_at", { withTimezone: true }).notNull().defaultNow(),
}, (t) => [primaryKey({ columns: [t.postId, t.userId] })]);

export const organizationsRelations = relations(organizations, ({ many }) => ({ users: many(users) }));
export const usersRelations = relations(users, ({ one, many }) => ({
  organization: one(organizations, { fields: [users.orgId], references: [organizations.id] }),
  posts: many(posts),
}));
export const postsRelations = relations(posts, ({ one, many }) => ({
  author: one(users, { fields: [posts.authorId], references: [users.id] }),
  comments: many(comments),
}));
export const commentsRelations = relations(comments, ({ one }) => ({
  post: one(posts, { fields: [comments.postId], references: [posts.id] }),
  author: one(users, { fields: [comments.authorId], references: [users.id] }),
}));
