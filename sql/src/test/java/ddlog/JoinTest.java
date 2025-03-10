/*
 * Copyright (c) 2021 VMware, Inc.
 * SPDX-License-Identifier: MIT
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */

package ddlog;

import org.junit.Test;

public class JoinTest extends BaseQueriesTest {
    @Test
    public void testCountJoin() {
        String query = "create view v0 as SELECT COUNT(t1.column2) as ct FROM t1 JOIN t2 ON t1.column1 = t2.column1";
        String program = this.header(false) +
                "typedef TRtmp = TRtmp{ct:signed<64>}\n" +
                "function agg(g: Group<(), TRt1>):TRtmp {\n" +
                "var count = 64'sd0: signed<64>;\n" +
                "(for ((i, _) in g) {\n" +
                "var v1 = i;\n" +
                "(var incr = v1.column2);\n" +
                "(count = agg_count_R(count, incr))}\n" +
                ");\n" +
                "(TRtmp{.ct = count})\n}\n" +
                this.relations(false) +
                "relation Rtmp[TRtmp]\n" +
                "output relation Rv0[TRtmp]\n" +
                "Rv0[v3] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column1}],var v1 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},var groupResult = (v1).group_by(()),var aggResult = agg(groupResult),var v2 = aggResult,var v3 = v2.";
        this.testTranslation(query, program);
    }

    @Test
    public void testImplicitJoin() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1, t2";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column10:signed<64>}\n" +
                this.relations(false) +
                "output relation Rv0[Ttmp]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column10}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column10 = column10},var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testJoinStar() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 JOIN t2 ON t1.column1 = t2.column1";
        String program = this.header(false) +
                this.relations(false) +
                "output relation Rv0[TRt1]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column1}],var v1 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testJoinMix() {
        // mixing nulls and non-nulls
        String query = "create view v0 as SELECT DISTINCT * FROM t1 JOIN t4 ON t1.column1 = t4.column1";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column10:Option<signed<64>>, column20:Option<string>}\n" +
                this.relations(false) +
                "output relation Rv0[Ttmp]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt4[TRt4{.column1 = Some{.x = column1},.column2 = column20}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column10 = Some{.x = column1},.column20 = column20},var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testJoinStarWNull() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 JOIN t2 ON t1.column1 = t2.column1";
        String program = this.header(true) +
                this.relations(true) +
                "output relation Rv0[TRt1]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = Some{.x = column1},.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = Some{.x = column1}}],var v1 = TRt1{.column1 = Some{.x = column1},.column2 = column2,.column3 = column3,.column4 = column4},var v2 = v1.";
        this.testTranslation(query, program, true);
    }

    @Test
    public void testSelfJoin() {
        String query = "create view v0 as SELECT DISTINCT t1.column2, x.column3 FROM t1 JOIN (t1 AS x) ON t1.column1 = x.column1";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column20:string, column30:bool, column40:double}\n" +
                "typedef TRx = TRx{column2:string, column3:bool}\n" +
                this.relations(false) +
                "output relation Rv0[TRx]\n" +
                "Rv0[v3] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt1[TRt1{.column1 = column1,.column2 = column20,.column3 = column30,.column4 = column40}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column20 = column20,.column30 = column30,.column40 = column40},var v2 = TRx{.column2 = v1.column2,.column3 = v1.column30},var v3 = v2.";
                this.testTranslation(query, program, false);
    }

    @Test
    public void testNaturalJoin() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 NATURAL JOIN t2";
        String program = this.header(false) +
                this.relations(false) +
                "output relation Rv0[TRt1]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column1}],var v1 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testNaturalJoinWhere() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 NATURAL JOIN t2 WHERE column3";
        String program = this.header(false) +
                this.relations(false) +
                "output relation Rv0[TRt1]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column1}],var v1 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},v1.column3,var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testJoin() {
        String query = "create view v0 as SELECT DISTINCT t0.column1, t1.column3 FROM t1 AS t0 JOIN t1 ON t1.column2 = t0.column2";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column10:signed<64>, column30:bool, column40:double}\n" +
                "typedef TRt0 = TRt0{column1:signed<64>, column3:bool}\n" +
                this.relations(false) +
                "output relation Rv0[TRt0]\n" +
                "Rv0[v3] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt1[TRt1{.column1 = column10,.column2 = column2,.column3 = column30,.column4 = column40}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column10 = column10,.column30 = column30,.column40 = column40},var v2 = TRt0{.column1 = v1.column1,.column3 = v1.column30},var v3 = v2.";
        this.testTranslation(query, program);
    }

    @Test
    public void testCrossJoin() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 CROSS JOIN t2";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column10:signed<64>}\n" +
                this.relations(false) +
                "output relation Rv0[Ttmp]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column10}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column10 = column10},var v2 = v1.";
        this.testTranslation(query, program);
    }

    @Test
    public void testCrossJoinWNull() {
        String query = "create view v0 as SELECT DISTINCT * FROM t1 CROSS JOIN t2";
        String program = this.header(true) +
                "typedef Ttmp = Ttmp{column1:Option<signed<64>>, column2:Option<string>, column3:Option<bool>, column4:Option<double>, column10:Option<signed<64>>}\n" +
                this.relations(true) +
                "output relation Rv0[Ttmp]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column10}],var v1 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.column10 = column10},var v2 = v1.";
        this.testTranslation(query, program, true);
    }

    @Test
    public void testJoinSubquery() {
        String query = "create view v0 as SELECT DISTINCT t1.column1, X.c FROM t1 CROSS JOIN (SELECT DISTINCT column1 AS c FROM t2 AS X)";
        String program = this.header(false) +
                "typedef TX = TX{c:signed<64>}\n" +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, c:signed<64>}\n" +
                "typedef TRtmp0 = TRtmp0{column1:signed<64>, c:signed<64>}\n" +
                this.relations(true) +
                "relation Rtmp[TX]\n" +
                "output relation Rv0[TRtmp0]\n" +
                "Rtmp[v2] :- Rt2[v0],var v1 = TX{.c = v0.column1},var v2 = v1.\n" +
                "Rv0[v5] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rtmp[TX{.c = c}],var v3 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.c = c},var v4 = TRtmp0{.column1 = v3.column1,.c = v3.c},var v5 = v4.";
        this.testTranslation(query, program, false);
    }

    @Test
    public void testNonEquiJoin() {
        String query = "create view v0 as SELECT DISTINCT t1.column2, t2.column1 FROM " +
                "t1 JOIN t2 ON t1.column1 < t2.column1";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, column10:signed<64>}\n" +
                "typedef TRtmp = TRtmp{column2:string, column1:signed<64>}\n" +
                this.relations(true) +
                "output relation Rv0[TRtmp]\n" +
                "Rv0[v3] :- Rt1[v],Rt2[v0],(v.column1 < v0.column1)," +
                "var v1 = Ttmp{.column1 = v.column1,.column2 = v.column2,.column3 = v.column3,.column4 = v.column4," +
                ".column10 = v0.column1},var v2 = TRtmp{.column2 = v1.column2,.column1 = v1.column10},var v3 = v2.";
        this.testTranslation(query, program, false);
    }

    @Test
    public void testNonEquiJoin2() {
        String query = "create view v0 as SELECT DISTINCT t1.column2, t4.column1 FROM " +
                "t1 JOIN t4 ON t1.column1 < t4.column1";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, " +
                "column10:Option<signed<64>>, column20:Option<string>}\n" +
                "typedef TRtmp = TRtmp{column2:string, column1:Option<signed<64>>}\n" +
                this.relations(true) +
                "output relation Rv0[TRtmp]\n" +
                "Rv0[v3] :- Rt1[v],Rt4[v0],unwrapBool(a_lt_RN(v.column1, v0.column1))," +
                "var v1 = Ttmp{.column1 = v.column1,.column2 = v.column2,.column3 = v.column3,.column4 = v.column4," +
                ".column10 = v0.column1,.column20 = v0.column2}," +
                "var v2 = TRtmp{.column2 = v1.column2,.column1 = v1.column10},var v3 = v2.";
        this.testTranslation(query, program, false);
    }

    @Test
    public void testInnerJoinSubquery() {
        String query = "create view v0 as SELECT DISTINCT t1.column1, X.c FROM t1 " +
                "JOIN ((SELECT DISTINCT column1 AS c FROM t2) AS X) " +
                "ON t1.column1 = X.c WHERE column2 = 'a'";
        String program = this.header(false) +
                "typedef TX = TX{c:signed<64>}\n" +
                "typedef TRtmp0 = TRtmp0{column1:signed<64>, c:signed<64>}\n" +
                this.relations(true) +
                "relation Rtmp[TX]\n" +
                "output relation Rv0[TRtmp0]\n" +
                "Rtmp[v2] :- Rt2[v0],var v1 = TX{.c = v0.column1},var v2 = v1.\n" +
                "Rv0[v5] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rtmp[TX{.c = column1}],var v3 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},(v3.column2 == \"a\"),var v4 = TRtmp0{.column1 = v3.column1,.c = v3.column1},var v5 = v4.";
        this.testTranslation(query, program, false);
    }

    @Test
    public void testMultiJoin() {
        String query = "create view v0 as SELECT DISTINCT *\n" +
                "    FROM t1,\n" +
                "         (SELECT DISTINCT column1 AS a FROM t1) b,\n" +
                "         (SELECT DISTINCT column2 AS c FROM t1) c,\n" +
                "         (SELECT DISTINCT column3 AS d FROM t1) d";
        String program = this.header(false) +
                "typedef TRb = TRb{a:signed<64>}\n" +
                "typedef Ttmp = Ttmp{column1:signed<64>, column2:string, column3:bool, column4:double, a:signed<64>}\n" +
                "typedef TRc = TRc{c:string}\n" +
                "typedef Ttmp0 = Ttmp0{column1:signed<64>, column2:string, column3:bool, column4:double, a:signed<64>, c:string}\n" +
                "typedef TRd = TRd{d:bool}\n" +
                "typedef Ttmp1 = Ttmp1{column1:signed<64>, column2:string, column3:bool, column4:double, a:signed<64>, c:string, d:bool}\n" +
                this.relations(false) +
                "relation Rtmp[TRb]\n" +
                "relation Rtmp0[TRc]\n" +
                "relation Rtmp1[Ttmp]\n" +
                "relation Rtmp2[TRd]\n" +
                "relation Rtmp3[Ttmp0]\n" +
                "output relation Rv0[Ttmp1]\n" +
                "Rtmp[v2] :- Rt1[v0],var v1 = TRb{.a = v0.column1},var v2 = v1.\n" +
                "Rtmp0[v6] :- Rt1[v4],var v5 = TRc{.c = v4.column2},var v6 = v5.\n" +
                "Rtmp1[v7] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rtmp[TRb{.a = a}],var v3 = Ttmp{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4,.a = a},var v7 = v3.\n" +
                "Rtmp2[v11] :- Rt1[v9],var v10 = TRd{.d = v9.column3},var v11 = v10.\n" +
                "Rtmp3[v12] :- Rtmp1[Ttmp{.column1 = column10,.column2 = column20,.column3 = column30,.column4 = column40,.a = a0}],Rtmp0[TRc{.c = c}],var v8 = Ttmp0{.column1 = column10,.column2 = column20,.column3 = column30,.column4 = column40,.a = a0,.c = c},var v12 = v8.\n" +
                "Rv0[v14] :- Rtmp3[Ttmp0{.column1 = column11,.column2 = column21,.column3 = column31,.column4 = column41,.a = a1,.c = c0}],Rtmp2[TRd{.d = d}],var v13 = Ttmp1{.column1 = column11,.column2 = column21,.column3 = column31,.column4 = column41,.a = a1,.c = c0,.d = d},var v14 = v13.";
        this.testTranslation(query, program);
    }

    @Test
    public void test2WayJoin() {
        String query = "create view v0 as SELECT DISTINCT t2.column1, t4.column1 AS x FROM (t2 JOIN t4 ON t2.column1 = t4.column1)";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column10:Option<signed<64>>, column2:Option<string>}\n" +
                "typedef TRtmp = TRtmp{column1:signed<64>, x:Option<signed<64>>}\n" +
                this.relations(false) +
                "output relation Rv0[TRtmp]\n" +
                "Rv0[v3] :- Rt2[TRt2{.column1 = column1}],Rt4[TRt4{.column1 = Some{.x = column1},.column2 = column2}],var v1 = Ttmp{.column1 = column1,.column10 = Some{.x = column1},.column2 = column2},var v2 = TRtmp{.column1 = v1.column1,.x = v1.column10},var v3 = v2.";
        this.testTranslation(query, program);
    }

    @Test
    public void test3WayJoin() {
        String query = "create view v0 as SELECT DISTINCT t1.column2, t2.column1, t4.column1 AS x FROM t1 JOIN " +
                "(t2 JOIN t4 ON t2.column1 = t4.column1) ON t1.column1 = t2.column1";
        String program = this.header(false) +
                "typedef Ttmp = Ttmp{column1:signed<64>, column10:Option<signed<64>>, column2:Option<string>}\n" +
                "typedef Ttmp0 = Ttmp0{column1:signed<64>, column2:string, column3:bool, column4:double, column100:Option<signed<64>>, column20:Option<string>}\n" +
                "typedef TRtmp0 = TRtmp0{column2:string, column1:signed<64>, x:Option<signed<64>>}\n" +
                this.relations(false) +
                "relation Rtmp[Ttmp]\n" +
                "output relation Rv0[TRtmp0]\n" +
                "Rtmp[v3] :- Rt2[TRt2{.column1 = column1}],Rt4[TRt4{.column1 = Some{.x = column1},.column2 = column2}],var v2 = Ttmp{.column1 = column1,.column10 = Some{.x = column1},.column2 = column2},var v3 = v2.\n" +
                "Rv0[v6] :- Rt1[TRt1{.column1 = column11,.column2 = column20,.column3 = column3,.column4 = column4}],Rtmp[Ttmp{.column1 = column11,.column10 = column100,.column2 = column21}],var v4 = Ttmp0{.column1 = column11,.column2 = column20,.column3 = column3,.column4 = column4,.column100 = column100,.column20 = column21},var v5 = TRtmp0{.column2 = v4.column2,.column1 = v4.column1,.x = v4.column100},var v6 = v5.";
        this.testTranslation(query, program);
    }

    @Test
    public void testLeftJoin() {
        // TODO: this is not complete.
        String query = "create view v0 as SELECT DISTINCT * FROM t1 LEFT JOIN t2 ON t1.column1 = t2.column1";
        String program = this.header(false) +
                this.relations(false) +
                "output relation Rv0[TRt1]\n" +
                "Rv0[v2] :- Rt1[TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4}],Rt2[TRt2{.column1 = column1}],var v1 = TRt1{.column1 = column1,.column2 = column2,.column3 = column3,.column4 = column4},var v2 = v1.";
        this.testTranslation(query, program);
    }
}
