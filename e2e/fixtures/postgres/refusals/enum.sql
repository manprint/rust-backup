-- M-PG-REF-01: an enum type cannot be reproduced 1:1 by this build.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE TYPE ref_mood AS ENUM ('low', 'mid', 'high');
