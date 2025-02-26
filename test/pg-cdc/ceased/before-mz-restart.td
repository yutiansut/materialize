# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

> CREATE SECRET pgpass AS 'postgres'

> CREATE CONNECTION pg TO POSTGRES (
    HOST postgres,
    DATABASE postgres,
    USER postgres,
    PASSWORD SECRET pgpass
  )

$ postgres-execute connection=postgres://postgres:postgres@postgres
ALTER USER postgres WITH replication;
DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;

DROP PUBLICATION IF EXISTS mz_source;
CREATE PUBLICATION mz_source FOR ALL TABLES;

CREATE TABLE t (a int);
INSERT INTO t VALUES (1);
ALTER TABLE t REPLICA IDENTITY FULL;

> CREATE SOURCE pg_source
  FROM POSTGRES CONNECTION pg (PUBLICATION 'mz_source')
  FOR TABLES ("t");

> SELECT * FROM t;
1

$ postgres-execute connection=postgres://postgres:postgres@postgres
ALTER TABLE t DROP COLUMN a;
ALTER TABLE t ADD COLUMN a int;
INSERT INTO t VALUES (2);

> SELECT true FROM mz_internal.mz_source_statuses WHERE name = 't' AND status = 'ceased' AND error ILIKE 'incompatible schema change%';
true

$ postgres-execute connection=postgres://postgres:postgres@postgres
DROP PUBLICATION mz_source;
INSERT INTO t VALUES (1);

# Primary source detects publication dropped as ceasing error
> SELECT true
    FROM mz_internal.mz_source_statuses
    WHERE
        name = 'pg_source' AND status = 'ceased'
            AND
        error = 'publication "mz_source" does not exist';
true

# Subsource does not have its status changed
> SELECT true
    FROM mz_internal.mz_source_statuses
    WHERE
        name = 't' AND status = 'ceased'
            AND
        error ILIKE 'incompatible schema change%';
true
