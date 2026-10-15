ALTER TABLE "posts" ADD CONSTRAINT "posts_price_nonneg" CHECK ("price" IS NULL OR "price" >= 0);--> statement-breakpoint
ALTER TABLE "users" ADD CONSTRAINT "users_karma_nonneg" CHECK ("karma" >= 0);--> statement-breakpoint
ALTER TABLE "posts" ALTER COLUMN "title" SET DATA TYPE varchar(200);--> statement-breakpoint
CREATE INDEX "posts_tags_gin_idx" ON "posts" USING gin ("tags");--> statement-breakpoint
CREATE INDEX "posts_published_partial_idx" ON "posts" ("created_at" DESC) WHERE "published";
