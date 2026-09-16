-- M-PG-GIS-01 .. 05. Loaded only when RB_PG_IMAGE_REPO is postgis/postgis,
-- from this subdirectory, after every file of the parent directory.

-- GIS-01
CREATE EXTENSION postgis;

-- GIS-02: geometry and geography columns with rows.
CREATE TABLE mx.t_gis_02 (
  id   integer PRIMARY KEY,
  geom geometry(Point, 4326),
  geog geography(Point, 4326)
);
INSERT INTO mx.t_gis_02
SELECT g,
       ST_SetSRID(ST_MakePoint(9.19 + g * 0.01, 45.46 + g * 0.01), 4326),
       ST_SetSRID(ST_MakePoint(9.19 + g * 0.01, 45.46 + g * 0.01), 4326)::geography
FROM generate_series(1, 10) g;

-- GIS-03
CREATE INDEX i_gis_03 ON mx.t_gis_02 USING gist (geom);

-- GIS-04: a custom row in the extension's configuration table spatial_ref_sys.
INSERT INTO spatial_ref_sys (srid, auth_name, auth_srid, srtext, proj4text)
VALUES (990001, 'RB', 990001,
  'GEOGCS["RB",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]',
  '+proj=longlat +datum=WGS84 +no_defs');

-- GIS-05
CREATE VIEW mx.v_gis_05 AS SELECT id, ST_AsText(geom) AS wkt FROM mx.t_gis_02;
